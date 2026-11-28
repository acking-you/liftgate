//! Dedicated interactive Auto-Update Manager TUI (`lg auto-update` / `lg au`, Cockpit `[v]`,
//! and `lg top` `[u]`).
//!
//! Extracted into its own crate depending only on `liftgate-auto-update`, `liftgate-types`,
//! `ratatui`, `crossterm`, and `tokio` so it compiles in parallel with `liftgate-server` and
//! `liftgate-core`, and links once as an `rlib` across both `liftgate` and `lg` binaries.

#![forbid(unsafe_code)]

use std::future::Future;
use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use liftgate_auto_update::{
    AutoUpdateLedger, AutoUpdateProbeCache, AutoUpdateStatusReport, FleetSwitchSnapshot,
    IdleGateEvaluation, SingleTargetRunResult,
};
use liftgate_types::{
    config::AutoUpdateTarget,
    telemetry::{AutoUpdateLastOutcome, AutoUpdateTargetTelemetry},
};
use ratatui::{
    Frame, Terminal,
    backend::Backend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
};

const BRAILLE_SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Fast zero-subprocess initial state read synchronously from local disk before the background
/// probe worker finishes inspecting `--version` across installed binaries.
#[derive(Clone, Debug)]
pub struct AutoUpdateInitialView {
    pub switches: FleetSwitchSnapshot,
    pub ledger: AutoUpdateLedger,
    pub daemon_running: bool,
    pub daemon_pid: Option<u32>,
    pub local_idle: IdleGateEvaluation,
    pub lg_version: String,
}

/// Host persistence and execution driver injected into [`run_auto_update_loop`].
pub trait AutoUpdateTuiController: Clone + Send + Sync + 'static {
    /// Reads local `settings.json`, `auto-update-state.json`, and `daemon.json` synchronously.
    fn initial_view(&self) -> AutoUpdateInitialView;

    /// Reads the latest switch snapshot from `settings.json`.
    fn current_switches(&self) -> FleetSwitchSnapshot;

    /// Flips `target`'s auto-update toggle, persists `settings.json`, and returns `(switches, new_value)`.
    fn toggle_target(
        &self,
        target: AutoUpdateTarget,
    ) -> Result<(FleetSwitchSnapshot, bool), String>;

    /// Steps the periodic check interval backward (`forward = false`) or forward (`forward = true`).
    fn step_interval(&self, forward: bool) -> Result<FleetSwitchSnapshot, String>;

    /// Enables idle auto-update for all 6 components (`lg`, `agy`, and all 4 coding harnesses).
    fn enable_all(&self) -> Result<FleetSwitchSnapshot, String>;

    /// Resets auto-update switches to defaults (`lg` & `agy` ON; coding harnesses OFF).
    fn reset_defaults(&self) -> Result<FleetSwitchSnapshot, String>;

    /// Inspects live daemon telemetry + cached binary version probes.
    fn inspect_cached(
        &self,
        cache: &mut AutoUpdateProbeCache,
        force_reprobe: bool,
    ) -> impl Future<Output = Result<AutoUpdateStatusReport, String>> + Send;

    /// Runs an on-demand check or update for `target`.
    fn run_single_target(
        &self,
        target: AutoUpdateTarget,
        check_only: bool,
    ) -> impl Future<Output = SingleTargetRunResult> + Send;

    /// Runs an on-demand update pass across all currently enabled targets.
    fn run_all_enabled(
        &self,
    ) -> impl Future<Output = Result<Vec<SingleTargetRunResult>, String>> + Send;
}

enum AutoUpdateWorkerCmd {
    RunSingle {
        target: AutoUpdateTarget,
        check_only: bool,
    },
    RunAllEnabled,
}

struct AutoUpdateManagerApp {
    lg_version: String,
    selected: usize,
    probing: bool,
    running_target: Option<(AutoUpdateTarget, bool)>,
    running_batch: bool,
    show_help: bool,
    anim_tick: usize,
    toast: Option<(String, bool, u64)>,
    daemon_running: bool,
    daemon_pid: Option<u32>,
    interval_mins: u64,
    idle_ready: bool,
    busy_reason: Option<String>,
    rows: Vec<AutoUpdateTargetTelemetry>,
}

impl AutoUpdateManagerApp {
    fn from_initial(init: AutoUpdateInitialView) -> Self {
        let rows = AutoUpdateTarget::ALL
            .into_iter()
            .map(|target| {
                let rec = init
                    .ledger
                    .targets
                    .get(&target)
                    .cloned()
                    .unwrap_or_default();
                let initial_ver = if target == AutoUpdateTarget::Liftgate {
                    Some(init.lg_version.clone())
                } else {
                    None
                };
                AutoUpdateTargetTelemetry {
                    target,
                    enabled: init.switches.is_enabled(target),
                    installed_version: initial_ver,
                    binary_path: None,
                    latest_seen_version: rec.latest_seen_version,
                    last_check_unix_ms: rec.last_check_unix_ms,
                    last_update_unix_ms: rec.last_update_unix_ms,
                    last_updated_from: rec.last_updated_from,
                    last_updated_to: rec.last_updated_to,
                    last_status: rec.last_status,
                    note: rec.note,
                }
            })
            .collect();

        let idle_ready = init.local_idle.is_idle();
        let busy_reason = init.local_idle.busy_reason().map(str::to_owned);

        Self {
            lg_version: init.lg_version,
            selected: 0,
            probing: true,
            running_target: None,
            running_batch: false,
            show_help: false,
            anim_tick: 0,
            toast: None,
            daemon_running: init.daemon_running,
            daemon_pid: init.daemon_pid,
            interval_mins: init.switches.interval_mins.get(),
            idle_ready,
            busy_reason,
            rows,
        }
    }

    fn apply_report(&mut self, switches: FleetSwitchSnapshot, report: AutoUpdateStatusReport) {
        self.probing = false;
        self.daemon_running = report.daemon_running;
        self.daemon_pid = report.daemon_pid;
        self.interval_mins = switches.interval_mins.get();
        self.idle_ready = report.idle_ready;
        self.busy_reason = report.busy_reason;
        self.rows = report
            .targets
            .into_iter()
            .map(|mut r| {
                r.enabled = switches.is_enabled(r.target);
                r
            })
            .collect();
        if report.sweep_in_progress
            && self.running_target.is_none()
            && let Some(t) = report.active_target
        {
            self.running_target = Some((t, false));
        }
    }

    fn set_toast(&mut self, msg: impl Into<String>, ok: bool) {
        self.toast = Some((msg.into(), ok, now_unix_ms()));
    }

    fn selected_target(&self) -> AutoUpdateTarget {
        self.rows
            .get(self.selected)
            .map_or(AutoUpdateTarget::Liftgate, |r| r.target)
    }

    fn sync_switches(&mut self, switches: FleetSwitchSnapshot) {
        self.interval_mins = switches.interval_mins.get();
        for row in &mut self.rows {
            row.enabled = switches.is_enabled(row.target);
        }
    }
}

/// Runs the full-screen interactive Auto-Update Manager event loop on `terminal`.
pub async fn run_auto_update_loop<B, C>(terminal: &mut Terminal<B>, controller: C) -> io::Result<()>
where
    B: Backend<Error = io::Error>,
    C: AutoUpdateTuiController,
{
    let mut app = AutoUpdateManagerApp::from_initial(controller.initial_view());
    let (refresh_tx, mut refresh_rx) = watch::channel::<(u64, bool)>((0, true));
    let (report_tx, mut report_rx) = mpsc::channel::<AutoUpdateStatusReport>(4);
    let (worker_tx, mut worker_rx) = mpsc::channel::<AutoUpdateWorkerCmd>(8);
    let (done_tx, mut done_rx) = mpsc::channel::<(String, bool)>(8);

    let mut tasks = JoinSet::new();
    {
        let poll_ctl = controller.clone();
        tasks.spawn(async move {
            let mut probe_cache = AutoUpdateProbeCache::default();
            let mut force_reprobe = true;
            loop {
                if let Ok(rep) = poll_ctl
                    .inspect_cached(&mut probe_cache, force_reprobe)
                    .await
                {
                    let _ = report_tx.send(rep).await;
                }
                force_reprobe = false;
                tokio::select! {
                    biased;
                    res = refresh_rx.changed() => {
                        if res.is_err() {
                            break;
                        }
                        force_reprobe = refresh_rx.borrow().1;
                    }
                    () = tokio::time::sleep(Duration::from_secs(3)) => {}
                }
            }
        });
    }

    {
        let act_ctl = controller.clone();
        let act_refresh = refresh_tx.clone();
        tasks.spawn(async move {
            while let Some(cmd) = worker_rx.recv().await {
                match cmd {
                    AutoUpdateWorkerCmd::RunSingle { target, check_only } => {
                        let res = act_ctl.run_single_target(target, check_only).await;
                        let ok = !matches!(res.status, AutoUpdateLastOutcome::Failed);
                        let _ = done_tx
                            .send((format!("{}: {}", target.display_name(), res.note), ok))
                            .await;
                    }
                    AutoUpdateWorkerCmd::RunAllEnabled => match act_ctl.run_all_enabled().await {
                        Ok(list) => {
                            let updated_cnt = list
                                .iter()
                                .filter(|r| r.status == AutoUpdateLastOutcome::Updated)
                                .count();
                            let failed_cnt = list
                                .iter()
                                .filter(|r| r.status == AutoUpdateLastOutcome::Failed)
                                .count();
                            let msg = format!(
                                "Checked {} enabled target(s): {} updated, {} failed",
                                list.len(),
                                updated_cnt,
                                failed_cnt
                            );
                            let _ = done_tx.send((msg, failed_cnt == 0)).await;
                        }
                        Err(err) => {
                            let _ = done_tx.send((err, false)).await;
                        }
                    },
                }
                let next = act_refresh.borrow().0.wrapping_add(1);
                let _ = act_refresh.send_replace((next, true));
            }
        });
    }

    loop {
        while let Ok(rep) = report_rx.try_recv() {
            app.apply_report(controller.current_switches(), rep);
        }
        while let Ok((msg, ok)) = done_rx.try_recv() {
            app.running_target = None;
            app.running_batch = false;
            app.set_toast(msg, ok);
        }
        if let Some((_, _, ts)) = &app.toast
            && now_unix_ms().saturating_sub(*ts) > 5_500
        {
            app.toast = None;
        }

        app.anim_tick = app.anim_tick.wrapping_add(1);
        terminal.draw(|frame| draw_auto_update_manager(frame, &app))?;

        if handle_auto_update_events(&controller, &mut app, &refresh_tx, &worker_tx)? {
            break;
        }

        tokio::time::sleep(Duration::from_millis(45)).await;
    }

    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    Ok(())
}

fn handle_auto_update_events<C: AutoUpdateTuiController>(
    controller: &C,
    app: &mut AutoUpdateManagerApp,
    refresh_tx: &watch::Sender<(u64, bool)>,
    worker_tx: &mpsc::Sender<AutoUpdateWorkerCmd>,
) -> io::Result<bool> {
    while event::poll(Duration::ZERO)? {
        let Event::Key(key) = event::read()? else {
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

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
            KeyCode::Char('?') => app.show_help = true,
            KeyCode::Up | KeyCode::Char('k') => {
                app.selected = if app.selected == 0 {
                    app.rows.len().saturating_sub(1)
                } else {
                    app.selected.saturating_sub(1)
                };
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !app.rows.is_empty() {
                    app.selected = (app.selected + 1) % app.rows.len();
                }
            }
            KeyCode::Char(d @ '1'..='6') => {
                let idx = (d as usize).saturating_sub('1' as usize);
                if idx < app.rows.len() {
                    app.selected = idx;
                }
            }
            KeyCode::Char(' ') | KeyCode::Enter => {
                let target = app.selected_target();
                match controller.toggle_target(target) {
                    Ok((switches, next_val)) => {
                        app.sync_switches(switches);
                        app.set_toast(
                            format!(
                                "{} idle auto-update for {}",
                                if next_val { "Enabled" } else { "Disabled" },
                                target.display_name()
                            ),
                            true,
                        );
                        trigger_refresh(refresh_tx, false);
                    }
                    Err(err) => app.set_toast(err, false),
                }
            }
            KeyCode::Char('[') | KeyCode::Char('-') => match controller.step_interval(false) {
                Ok(switches) => {
                    app.sync_switches(switches);
                    app.set_toast(
                        format!(
                            "Idle check interval set to every {}m",
                            switches.interval_mins.get()
                        ),
                        true,
                    );
                    trigger_refresh(refresh_tx, false);
                }
                Err(err) => app.set_toast(err, false),
            },
            KeyCode::Char(']') | KeyCode::Char('+') | KeyCode::Char('=') => {
                match controller.step_interval(true) {
                    Ok(switches) => {
                        app.sync_switches(switches);
                        app.set_toast(
                            format!(
                                "Idle check interval set to every {}m",
                                switches.interval_mins.get()
                            ),
                            true,
                        );
                        trigger_refresh(refresh_tx, false);
                    }
                    Err(err) => app.set_toast(err, false),
                }
            }
            KeyCode::Char('a') => match controller.enable_all() {
                Ok(switches) => {
                    app.sync_switches(switches);
                    app.set_toast("Enabled idle auto-update for all 6 components", true);
                    trigger_refresh(refresh_tx, false);
                }
                Err(err) => app.set_toast(err, false),
            },
            KeyCode::Char('d') => match controller.reset_defaults() {
                Ok(switches) => {
                    app.sync_switches(switches);
                    app.set_toast("Reset to defaults: lg & agy ON, coding harnesses OFF", true);
                    trigger_refresh(refresh_tx, false);
                }
                Err(err) => app.set_toast(err, false),
            },
            KeyCode::Char('c') => {
                if app.running_target.is_none() && !app.running_batch {
                    let target = app.selected_target();
                    app.running_target = Some((target, true));
                    app.set_toast(format!("Checking {}…", target.display_name()), true);
                    let _ = worker_tx.try_send(AutoUpdateWorkerCmd::RunSingle {
                        target,
                        check_only: true,
                    });
                }
            }
            KeyCode::Char('u') => {
                if app.running_target.is_none() && !app.running_batch {
                    let target = app.selected_target();
                    app.running_target = Some((target, false));
                    app.set_toast(format!("Updating {}…", target.display_name()), true);
                    let _ = worker_tx.try_send(AutoUpdateWorkerCmd::RunSingle {
                        target,
                        check_only: false,
                    });
                }
            }
            KeyCode::Char('U') => {
                if app.running_target.is_none() && !app.running_batch {
                    app.running_batch = true;
                    app.set_toast("Running update pass for all enabled targets…", true);
                    let _ = worker_tx.try_send(AutoUpdateWorkerCmd::RunAllEnabled);
                }
            }
            KeyCode::Char('r') => {
                app.probing = true;
                app.set_toast("Refreshing installed versions & live status…", true);
                trigger_refresh(refresh_tx, true);
            }
            _ => {}
        }
    }
    Ok(false)
}

fn trigger_refresh(refresh_tx: &watch::Sender<(u64, bool)>, force_reprobe: bool) {
    let next = refresh_tx.borrow().0.wrapping_add(1);
    let _ = refresh_tx.send_replace((next, force_reprobe));
}

fn draw_auto_update_manager(frame: &mut Frame<'_>, app: &AutoUpdateManagerApp) {
    let root = centered_rect(108, 30, frame.area());
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(10),
            Constraint::Length(8),
            Constraint::Length(2),
        ])
        .split(root);

    let spin = BRAILLE_SPINNER
        .get((app.anim_tick / 2) % BRAILLE_SPINNER.len())
        .copied()
        .unwrap_or("⠋");

    if let Some(&header_area) = sections.first() {
        let daemon_badge = if app.daemon_running {
            Span::styled(
                format!(
                    " ● DAEMON ONLINE{} ",
                    app.daemon_pid
                        .map_or_else(String::new, |p| format!(" (PID {p})"))
                ),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(
                " ○ DAEMON OFFLINE (start with `lg up`) ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        };

        let gate_badge = if app.idle_ready {
            Span::styled(
                " ● SESSION GATE: IDLE (0 active sessions — auto-checks ready) ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            let why = app.busy_reason.as_deref().unwrap_or("active session");
            Span::styled(
                format!(" ⏸ SESSION GATE: BUSY ({why} — checks paused) "),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        };

        let enabled_cnt = app.rows.iter().filter(|r| r.enabled).count();
        let interval_txt = format_interval_mins(app.interval_mins);
        let header_lines = vec![
            Line::from(vec![daemon_badge, Span::raw("  "), gate_badge]),
            Line::from(vec![
                Span::styled(" Check Interval: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("Every {interval_txt}"),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" ([-/+] adjust)", Style::default().fg(Color::DarkGray)),
                Span::styled("   Enabled: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{enabled_cnt} / {}", app.rows.len()),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "   Policy: Checks & updates ONLY run when zero sessions are active",
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
        ];
        let header = Paragraph::new(header_lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Cyan))
                .title(Line::from(vec![
                    Span::styled(
                        " LIFTGATE · AUTO-UPDATE MANAGER ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("v{} ", app.lg_version),
                        Style::default().fg(Color::DarkGray),
                    ),
                ])),
        );
        frame.render_widget(header, header_area);
    }

    if let Some(&table_area) = sections.get(1) {
        let now = now_unix_ms();
        let table_rows: Vec<Row<'_>> = app
            .rows
            .iter()
            .enumerate()
            .map(|(idx, row)| {
                let default_tag = if row.target.default_enabled() {
                    " (default: on)"
                } else {
                    " (default: off)"
                };
                let switch_cell = if row.enabled {
                    Cell::from(Line::from(vec![
                        Span::styled(
                            "● ON ",
                            Style::default()
                                .fg(Color::Green)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(default_tag, Style::default().fg(Color::DarkGray)),
                    ]))
                } else {
                    Cell::from(Line::from(vec![
                        Span::styled("○ OFF", Style::default().fg(Color::DarkGray)),
                        Span::styled(default_tag, Style::default().fg(Color::DarkGray)),
                    ]))
                };

                let installed_cell = match &row.installed_version {
                    Some(v) => Cell::from(format!("v{v}")).style(Style::default().fg(Color::White)),
                    None if app.probing => Cell::from(format!("{spin} probing…"))
                        .style(Style::default().fg(Color::DarkGray)),
                    None if row.binary_path.is_some() => {
                        Cell::from("installed").style(Style::default().fg(Color::Gray))
                    }
                    None => Cell::from("— missing").style(Style::default().fg(Color::DarkGray)),
                };

                let last_check = row
                    .last_check_unix_ms
                    .map_or_else(|| "never".to_owned(), |ts| format_relative_ms(ts, now));

                let is_busy_target = app.running_target.is_some_and(|(t, _)| t == row.target)
                    || (app.running_batch && row.enabled);
                let (status_txt, status_col) = if is_busy_target {
                    let verb = match app.running_target {
                        Some((_, true)) => "checking…",
                        _ => "updating…",
                    };
                    (format!("{spin} {verb}"), Color::Cyan)
                } else {
                    format_target_status_badge(row)
                };

                Row::new(vec![
                    Cell::from(format!("{}", idx + 1)).style(Style::default().fg(Color::DarkGray)),
                    Cell::from(row.target.display_name()).style(
                        Style::default()
                            .fg(if row.enabled {
                                Color::White
                            } else {
                                Color::Gray
                            })
                            .add_modifier(Modifier::BOLD),
                    ),
                    switch_cell,
                    installed_cell,
                    Cell::from(last_check).style(Style::default().fg(Color::Gray)),
                    Cell::from(status_txt).style(Style::default().fg(status_col)),
                ])
            })
            .collect();

        let mut tstate = TableState::default();
        if !app.rows.is_empty() {
            tstate.select(Some(app.selected.min(app.rows.len().saturating_sub(1))));
        }
        let table = Table::new(
            table_rows,
            [
                Constraint::Length(3),
                Constraint::Length(25),
                Constraint::Length(21),
                Constraint::Length(14),
                Constraint::Length(13),
                Constraint::Min(24),
            ],
        )
        .header(
            Row::new(vec![
                "#",
                "Component",
                "Idle Auto-Update",
                "Installed",
                "Last Check",
                "Status / Latest",
            ])
            .style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::Rgb(24, 38, 58))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ")
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(
                    " Managed Components (Space/Enter to toggle ON/OFF) ",
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )),
        );
        frame.render_stateful_widget(table, table_area, &mut tstate);
    }

    if let Some(&detail_area) = sections.get(2)
        && let Some(sel) = app.rows.get(app.selected)
    {
        let now = now_unix_ms();
        let bin_str = sel.binary_path.as_deref().unwrap_or("not detected on PATH");
        let last_upgrade = match (
            &sel.last_updated_from,
            &sel.last_updated_to,
            sel.last_update_unix_ms,
        ) {
            (Some(from), Some(to), Some(ts)) => {
                format!("v{from} -> v{to} ({})", format_relative_ms(ts, now))
            }
            (_, Some(to), Some(ts)) => {
                format!("-> v{to} ({})", format_relative_ms(ts, now))
            }
            _ => "none recorded yet".to_owned(),
        };
        let note_str = sel
            .note
            .as_deref()
            .unwrap_or("No checks recorded yet; daemon checks automatically when idle.");
        let default_desc = if sel.target.default_enabled() {
            "Enabled by default"
        } else {
            "Disabled by default (opt-in harness management)"
        };

        let detail_lines = vec![
            Line::from(vec![
                Span::styled(" Executable: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    single_line_clean(bin_str, 86),
                    Style::default().fg(Color::White),
                ),
            ]),
            Line::from(vec![
                Span::styled(" Strategy:   ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    sel.target.strategy_summary(),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled("   ·   ", Style::default().fg(Color::DarkGray)),
                Span::styled(default_desc, Style::default().fg(Color::Gray)),
            ]),
            Line::from(vec![
                Span::styled(" Last Apply: ", Style::default().fg(Color::DarkGray)),
                Span::styled(last_upgrade, Style::default().fg(Color::Green)),
            ]),
            Line::from(vec![
                Span::styled(" Detail:     ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    single_line_clean(note_str, 86),
                    Style::default().fg(Color::Gray),
                ),
            ]),
        ];
        let inspector = Paragraph::new(detail_lines)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::DarkGray))
                    .title(Span::styled(
                        format!(" Selected · {} ", sel.target.display_name()),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )),
            );
        frame.render_widget(inspector, detail_area);
    }

    if let Some(&footer_area) = sections.get(3) {
        let line = if let Some((msg, ok, _)) = &app.toast {
            let (glyph, col) = if *ok {
                ("✓ ", Color::Green)
            } else {
                ("⚠ ", Color::Yellow)
            };
            Line::from(vec![
                Span::styled(glyph, Style::default().fg(col).add_modifier(Modifier::BOLD)),
                Span::styled(msg.as_str(), Style::default().fg(col)),
            ])
        } else {
            Line::from(vec![
                Span::styled(
                    " ↑↓/jk ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("Select  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "Space/Enter ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("Toggle  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "[-/+] ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("Interval  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "c ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("Check  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "u/U ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("Update Target/All  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "a/d ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("All/Defaults  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "? ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("Help  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "q/Esc ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("Quit", Style::default().fg(Color::DarkGray)),
            ])
        };
        let footer = Paragraph::new(line).alignment(Alignment::Center);
        frame.render_widget(footer, footer_area);
    }

    if app.show_help {
        draw_auto_update_help_modal(frame);
    }
}

fn draw_auto_update_help_modal(frame: &mut Frame<'_>) {
    let area = centered_rect(68, 17, frame.area());
    frame.render_widget(Clear, area);
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "  ↑ / ↓ or k / j   ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Select component (or press 1..6)"),
        ]),
        Line::from(vec![
            Span::styled(
                "  Space / Enter    ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Toggle idle auto-update ON / OFF"),
        ]),
        Line::from(vec![
            Span::styled(
                "  [ / ] or - / +   ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Adjust periodic idle check interval (5m..24h)"),
        ]),
        Line::from(vec![
            Span::styled(
                "  c                ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Check selected component version now"),
        ]),
        Line::from(vec![
            Span::styled(
                "  u                ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Check & update selected component now"),
        ]),
        Line::from(vec![
            Span::styled(
                "  U                ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Check & update all enabled components now"),
        ]),
        Line::from(vec![
            Span::styled(
                "  a / d            ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Enable all / Reset defaults (lg & agy ON)"),
        ]),
        Line::from(vec![
            Span::styled(
                "  r                ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Re-probe installed binaries & daemon status"),
        ]),
        Line::from(vec![
            Span::styled(
                "  q / Esc / ?      ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("Close help / Exit Auto-Update Manager"),
        ]),
    ];
    let modal = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Span::styled(
                " Auto-Update Manager Shortcuts ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
    );
    frame.render_widget(modal, area);
}

fn centered_rect(max_width: u16, max_height: u16, area: Rect) -> Rect {
    let width = area.width.min(max_width);
    let height = area.height.min(max_height);
    let x = area.x.saturating_add(area.width.saturating_sub(width) / 2);
    let y = area
        .y
        .saturating_add(area.height.saturating_sub(height) / 2);
    Rect::new(x, y, width, height)
}

fn format_interval_mins(mins: u64) -> String {
    if mins >= 60 && mins.is_multiple_of(60) {
        format!("{}h", mins / 60)
    } else {
        format!("{mins}m")
    }
}

fn format_relative_ms(ts_unix_ms: u64, now_ms: u64) -> String {
    let elapsed_secs = now_ms.saturating_sub(ts_unix_ms) / 1_000;
    if elapsed_secs < 5 {
        "just now".to_owned()
    } else if elapsed_secs < 60 {
        format!("{elapsed_secs}s ago")
    } else if elapsed_secs < 3_600 {
        format!("{}m ago", elapsed_secs / 60)
    } else if elapsed_secs < 86_400 {
        format!("{}h ago", elapsed_secs / 3_600)
    } else {
        format!("{}d ago", elapsed_secs / 86_400)
    }
}

fn single_line_clean(raw: &str, max_chars: usize) -> String {
    let collapsed: String = raw
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let trimmed = collapsed.trim();
    if trimmed.chars().count() <= max_chars {
        trimmed.to_owned()
    } else {
        let mut out: String = trimmed.chars().take(max_chars.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn format_target_status_badge(row: &AutoUpdateTargetTelemetry) -> (String, Color) {
    match row.last_status {
        AutoUpdateLastOutcome::UpToDate => ("✓ up to date".to_owned(), Color::Green),
        AutoUpdateLastOutcome::UpdateAvailable => (
            row.latest_seen_version.as_deref().map_or_else(
                || "↑ update available".to_owned(),
                |v| format!("↑ v{v} available"),
            ),
            Color::Yellow,
        ),
        AutoUpdateLastOutcome::Updated => (
            match (&row.last_updated_from, &row.last_updated_to) {
                (Some(f), Some(t)) => format!("✓ updated v{f} -> v{t}"),
                (_, Some(t)) => format!("✓ updated -> v{t}"),
                _ => "✓ updated".to_owned(),
            },
            Color::Green,
        ),
        AutoUpdateLastOutcome::DeferredBusy => {
            ("⏸ deferred (session active)".to_owned(), Color::Yellow)
        }
        AutoUpdateLastOutcome::NotInstalled => ("— not installed".to_owned(), Color::DarkGray),
        AutoUpdateLastOutcome::Skipped => ("· skipped".to_owned(), Color::DarkGray),
        AutoUpdateLastOutcome::Failed => (
            row.note.as_deref().map_or_else(
                || "✗ failed".to_owned(),
                |n| format!("✗ {}", single_line_clean(n, 28)),
            ),
            Color::Red,
        ),
        AutoUpdateLastOutcome::NeverChecked | AutoUpdateLastOutcome::Unknown => {
            if row.binary_path.is_none() && row.installed_version.is_none() {
                ("— not installed".to_owned(), Color::DarkGray)
            } else if row.enabled {
                ("· awaiting idle check".to_owned(), Color::Gray)
            } else {
                ("· auto-update off".to_owned(), Color::DarkGray)
            }
        }
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;

    use super::*;

    #[test]
    fn renders_auto_update_manager_and_help_modal_without_panicking() {
        let init = AutoUpdateInitialView {
            switches: FleetSwitchSnapshot::default(),
            ledger: AutoUpdateLedger::default(),
            daemon_running: true,
            daemon_pid: Some(4242),
            local_idle: IdleGateEvaluation::Idle,
            lg_version: "0.2.23".to_owned(),
        };
        let mut app = AutoUpdateManagerApp::from_initial(init);
        let backend = TestBackend::new(120, 34);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| draw_auto_update_manager(frame, &app))
            .unwrap();
        app.show_help = true;
        terminal
            .draw(|frame| draw_auto_update_manager(frame, &app))
            .unwrap();
    }
}
