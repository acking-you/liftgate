//! Tab 01: Overview & Gateway Daemon Lifecycle (`概览与网关`).

use gpui_kit::base::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{IconName, Sizable as _};
use gpui_kit::*;
use liftgate_core::daemon::GatewayStatusMode;
use liftgate_core::format::{format_duration_secs, format_tokens};

use crate::app::DesktopApp;
use crate::i18n::{NavTab, tr};
use crate::theme::{
    BadgeTone, Palette, editorial_card, kpi_card, kv_row, pill_badge, section_header,
    sf_terminal_box,
};

impl DesktopApp {
    pub(crate) fn render_overview_tab(
        &self,
        p: &Palette,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let lang = self.lang;
        let report = self.gateway_report.as_ref();
        let snap = self
            .live_telemetry
            .as_ref()
            .or_else(|| report.and_then(|r| r.telemetry.as_ref()));

        let is_online = report.is_some_and(|r| r.healthy);
        let is_managed = report.is_some_and(|r| r.mode == GatewayStatusMode::Managed);
        let (status_label, status_tone) = match report.map(|r| r.mode) {
            Some(GatewayStatusMode::Managed) => (
                tr(lang, "运行中 · 托管守护进程", "RUNNING · MANAGED DAEMON"),
                BadgeTone::Ok,
            ),
            Some(GatewayStatusMode::External) => (
                tr(lang, "运行中 · 外部网关实例", "RUNNING · EXTERNAL INSTANCE"),
                BadgeTone::Info,
            ),
            _ => (
                tr(lang, "离线 · 未启动", "OFFLINE · STOPPED"),
                BadgeTone::Neutral,
            ),
        };

        let gateway_url = report
            .map(|r| r.gateway_url.clone())
            .unwrap_or_else(|| "http://127.0.0.1:8787".to_owned());
        let dashboard_url = report
            .map(|r| r.dashboard_url.clone())
            .unwrap_or_else(|| "http://127.0.0.1:8787".to_owned());
        let pid_text = report
            .and_then(|r| r.pid)
            .or_else(|| snap.and_then(|s| s.acp_process.pid))
            .map_or_else(|| "—".to_owned(), |pid| format!("PID {pid}"));
        let agy_ver = report
            .and_then(|r| r.agy_version.clone())
            .or_else(|| snap.and_then(|s| s.acp_process.agent_version.clone()))
            .unwrap_or_else(|| tr(lang, "未检测到 agy", "agy not detected").to_owned());

        // --- Hero Gateway Banner Card ---
        let url_for_copy = gateway_url.clone();
        let env_snippet = format!(
            "export ANTHROPIC_BASE_URL=\"{gateway_url}\"\nexport OPENAI_BASE_URL=\"{gateway_url}/v1\""
        );

        let hero_card = editorial_card(p).child(
            div()
                .w_full()
                .flex()
                .flex_wrap()
                .items_center()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(pill_badge(p, status_tone, status_label))
                        .child(pill_badge(
                            p,
                            BadgeTone::Brand,
                            format!("API {gateway_url}"),
                        ))
                        .child(pill_badge(p, BadgeTone::Neutral, pid_text))
                        .child(pill_badge(p, BadgeTone::Info, format!("agy {agy_ver}")))
                        .children(self.cfg_long_context.then(|| {
                            pill_badge(
                                p,
                                BadgeTone::Warn,
                                tr(lang, "1M 长上下文", "1M LONG CONTEXT"),
                            )
                        }))
                        .children(self.sse_connected.then(|| {
                            pill_badge(p, BadgeTone::Ok, tr(lang, "SSE 实时连接", "SSE LIVE"))
                        })),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(
                            Button::new("ov-start-or-restart")
                                .small()
                                .primary()
                                .loading(self.busy_gateway)
                                .icon(if is_online {
                                    IconName::RefreshCw
                                } else {
                                    IconName::Play
                                })
                                .label(if is_online {
                                    tr(lang, "应用参数并重启", "Apply & Restart")
                                } else {
                                    tr(lang, "启动本地网关", "Start Gateway")
                                })
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.action_start_gateway(is_online, window, cx);
                                })),
                        )
                        .child(
                            Button::new("ov-stop-gw")
                                .small()
                                .danger()
                                .disabled(!is_managed || self.busy_gateway)
                                .icon(IconName::Square)
                                .label(tr(lang, "停止网关", "Stop"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.action_stop_gateway(cx);
                                })),
                        )
                        .child(
                            Button::new("ov-open-web")
                                .small()
                                .outline()
                                .icon(IconName::Globe)
                                .label(tr(lang, "Web 控制台", "Web Dashboard"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.action_open_web_dashboard(cx);
                                })),
                        )
                        .child(
                            Button::new("ov-copy-url")
                                .small()
                                .ghost()
                                .icon(IconName::Copy)
                                .label(tr(lang, "复制地址", "Copy URL"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.copy_text(
                                        tr(this.lang, "网关地址", "Gateway URL"),
                                        url_for_copy.clone(),
                                        cx,
                                    );
                                })),
                        )
                        .child(
                            Button::new("ov-copy-env")
                                .small()
                                .ghost()
                                .icon(IconName::SquareTerminal)
                                .label(tr(lang, "复制环境变量", "Copy Env"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.copy_text(
                                        tr(this.lang, "环境变量片段", "Gateway Env"),
                                        env_snippet.clone(),
                                        cx,
                                    );
                                })),
                        ),
                ),
        );

        // --- 4-Column Editorial KPI Row ---
        let uptime_val = snap
            .map(|s| format_duration_secs(s.acp_process.uptime_secs))
            .unwrap_or_else(|| "—".to_owned());
        let sup_sub = snap
            .map(|s| format!("ACP: {}", s.acp_process.supervision_strategy))
            .unwrap_or_else(|| tr(lang, "等待网关守护进程启动", "Waiting for daemon").to_owned());

        let (pool_val, pool_sub) = if let Some(sp) = snap.map(|s| &s.session_pool) {
            let total_acq = sp.total_sessions_created + sp.total_sessions_reused;
            let hit_pct = (sp.total_sessions_reused * 100)
                .checked_div(total_acq)
                .unwrap_or(0);
            (
                format!(
                    "{} / {}",
                    sp.active_leased_sessions + sp.idle_cached_sessions,
                    sp.max_idle_sessions
                ),
                if lang.is_en() {
                    format!(
                        "{} active · {} idle · {hit_pct}% warm hit",
                        sp.active_leased_sessions, sp.idle_cached_sessions
                    )
                } else {
                    format!(
                        "运行 {} · 空闲 {} · 命中率 {hit_pct}%",
                        sp.active_leased_sessions, sp.idle_cached_sessions
                    )
                },
            )
        } else {
            (
                "0 / 16".to_owned(),
                tr(lang, "热会话前缀缓存池", "LRU prefix session pool").to_owned(),
            )
        };

        let (req_val, req_sub) = if let Some(tp) = snap.map(|s| &s.metrics) {
            (
                tp.total_requests.to_string(),
                if lang.is_en() {
                    format!(
                        "OA {} · Anth {} · Err {}",
                        tp.openai_requests, tp.anthropic_requests, tp.failed_requests
                    )
                } else {
                    format!(
                        "OpenAI {} · Anthropic {} · 失败 {}",
                        tp.openai_requests, tp.anthropic_requests, tp.failed_requests
                    )
                },
            )
        } else {
            (
                "0".to_owned(),
                "OpenAI /v1/responses + Anthropic /v1/messages".to_owned(),
            )
        };

        let (tok_val, tok_sub) = if let Some(tp) = snap.map(|s| &s.metrics) {
            let total_tok =
                tp.total_input_tokens + tp.total_output_tokens + tp.total_thought_tokens;
            (
                format_tokens(total_tok),
                if lang.is_en() {
                    format!(
                        "In {} · Out {} · Cache {}",
                        format_tokens(tp.total_input_tokens),
                        format_tokens(tp.total_output_tokens),
                        format_tokens(tp.total_cached_tokens)
                    )
                } else {
                    format!(
                        "输入 {} · 输出 {} · 缓存 {}",
                        format_tokens(tp.total_input_tokens),
                        format_tokens(tp.total_output_tokens),
                        format_tokens(tp.total_cached_tokens)
                    )
                },
            )
        } else {
            (
                "0".to_owned(),
                tr(
                    lang,
                    "输入 / 输出 / 缓存命中统计",
                    "Input / Output / Cache read",
                )
                .to_owned(),
            )
        };

        let kpi_row = div()
            .w_full()
            .flex()
            .flex_row()
            .gap_3()
            .child(kpi_card(
                p,
                tr(lang, "网关在线时长", "GATEWAY UPTIME"),
                uptime_val,
                sup_sub,
            ))
            .child(kpi_card(
                p,
                tr(lang, "热会话池容量", "WARM SESSION POOL"),
                pool_val,
                pool_sub,
            ))
            .child(kpi_card(
                p,
                tr(lang, "累计 API 请求", "TOTAL REQUESTS"),
                req_val,
                req_sub,
            ))
            .child(kpi_card(
                p,
                tr(lang, "累计 Token 吞吐", "TOKEN THROUGHPUT"),
                tok_val,
                tok_sub,
            ));

        // --- Left Card: Gateway Configuration Form ---
        let form_card = editorial_card(p)
            .flex_1()
            .child(section_header(
                p,
                tr(lang, "守护进程参数", "DAEMON PARAMETERS"),
                tr(lang, "网关守护进程参数", "Gateway Daemon Configuration"),
                Some(tr(
                    lang,
                    "完整对应 `lg start` / `lg serve` 参数；保存后将以当前桌面进程自托管拉起守护进程。",
                    "Maps 1:1 to `lg start` / `lg serve`; spawns a self-supervised daemon from this desktop binary.",
                )),
            ))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .gap_3()
                    .child(labeled_input(
                        p,
                        tr(lang, "API 监听地址 (--listen)", "API Listen (--listen)"),
                        Input::new(&self.in_listen).small().cleanable(true),
                    ))
                    .child(labeled_input(
                        p,
                        tr(lang, "独立面板地址 (--ui-listen, 可选)", "UI Listen (--ui-listen, opt)"),
                        Input::new(&self.in_ui_listen).small().cleanable(true),
                    )),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .gap_3()
                    .child(labeled_input(
                        p,
                        tr(lang, "默认模型 (--default-model)", "Default Model (--default-model)"),
                        Input::new(&self.in_default_model).small().cleanable(true),
                    ))
                    .child(labeled_input(
                        p,
                        tr(lang, "ACP 模式 (--acp-mode)", "ACP Mode (--acp-mode)"),
                        Input::new(&self.in_acp_mode).small().cleanable(true),
                    )),
            )
            .child(labeled_input(
                p,
                tr(
                    lang,
                    "网关访问密钥 (--api-key / AGY_GATEWAY_API_KEY，留空免鉴权)",
                    "Gateway API Key (--api-key / AGY_GATEWAY_API_KEY, blank = open loopback)",
                ),
                Input::new(&self.in_api_key)
                    .small()
                    .mask_toggle()
                    .cleanable(true),
            ))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .gap_3()
                    .child(labeled_input(
                        p,
                        tr(lang, "最大热会话数 (--max-idle-sessions)", "Max Idle Sessions"),
                        Input::new(&self.in_max_idle_sessions).small(),
                    ))
                    .child(labeled_input(
                        p,
                        tr(lang, "单轮超时秒数 (--turn-timeout-secs)", "Turn Timeout (secs)"),
                        Input::new(&self.in_turn_timeout_secs).small(),
                    )),
            )
            .child(
                div()
                    .w_full()
                    .pt_1()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        Switch::new("ov-sw-long-ctx")
                            .small()
                            .checked(self.cfg_long_context)
                            .label(tr(
                                lang,
                                "启用 1M 长上下文窗口 (--long-context)",
                                "1M Long Context (--long-context)",
                            ))
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.cfg_long_context = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        Switch::new("ov-sw-auto-upd")
                            .small()
                            .checked(self.cfg_agy_auto_update)
                            .label(tr(
                                lang,
                                "空闲自动更新 agy (--agy-auto-update)",
                                "Idle agy Auto-Update",
                            ))
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.cfg_agy_auto_update = *checked;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .pt_2()
                    .border_t_1()
                    .border_color(p.border)
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        Button::new("ov-hydrate-active")
                            .small()
                            .ghost()
                            .icon(IconName::RefreshCw)
                            .label(tr(lang, "回填运行中配置", "Reload Active Config"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                if let Some(rep) = this.gateway_report.clone() {
                                    this.hydrate_form_from_status(&rep, window, cx);
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        Button::new("ov-save-restart")
                            .small()
                            .primary()
                            .loading(self.busy_gateway)
                            .icon(IconName::Check)
                            .label(tr(lang, "保存并启动 / 重启守护进程", "Save & Start / Restart"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.action_start_gateway(true, window, cx);
                            })),
                    ),
            );

        // --- Right Card: Quick Launchers + Runtime Paths ---
        let state_dir_str = self.state_paths.root().display().to_string();
        let log_path_str = self.state_paths.log_file().display().to_string();
        let agy_bin_str = report
            .and_then(|r| r.agy_binary_path.as_ref().map(|b| b.display().to_string()))
            .unwrap_or_else(|| {
                tr(
                    lang,
                    "自动探测 PATH / 托管目录",
                    "Auto-discover PATH / managed",
                )
                .to_owned()
            });
        let proxy_str = report
            .and_then(|r| r.agy_proxy_display.clone())
            .unwrap_or_else(|| tr(lang, "直连（未启用代理）", "direct (off)").to_owned());

        let right_col = div()
            .flex_1()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                editorial_card(p)
                    .child(section_header(
                        p,
                        tr(lang, "零污染启动器", "ZERO-POLLUTION LAUNCHERS"),
                        tr(lang, "一键启动 AI 编码客户端", "One-Click AI Harness Launchers"),
                        Some(tr(
                            lang,
                            "零全局配置污染；若网关未启动将自动拉起并注入局部会话凭证。",
                            "Zero global config pollution; auto-starts gateway and scopes credentials to child session.",
                        )),
                    ))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .child(
                                Button::new("ov-quick-claude")
                                    .small()
                                    .primary()
                                    .loading(self.busy_launcher)
                                    .icon(IconName::SquareTerminal)
                                    .label("Claude Code CLI")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.action_launch_claude_cli(None, false, cx);
                                    })),
                            )
                            .child(
                                Button::new("ov-quick-codex")
                                    .small()
                                    .secondary()
                                    .loading(self.busy_launcher)
                                    .icon(IconName::Cpu)
                                    .label("Codex CLI")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.action_launch_codex_cli(None, false, cx);
                                    })),
                            )
                            .child(
                                Button::new("ov-quick-codex-app")
                                    .small()
                                    .outline()
                                    .loading(self.busy_launcher)
                                    .icon(IconName::LayoutDashboard)
                                    .label("Codex App")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.action_launch_codex_app(None, cx);
                                    })),
                            )
                            .child(
                                Button::new("ov-quick-claude-desktop")
                                    .small()
                                    .outline()
                                    .loading(self.busy_launcher)
                                    .icon(IconName::Bot)
                                    .label("Claude Desktop")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.action_launch_claude_desktop(false, None, cx);
                                    })),
                            )
                            .child(
                                Button::new("ov-goto-launchers")
                                    .small()
                                    .ghost()
                                    .icon(IconName::ArrowRight)
                                    .label(tr(lang, "高级参数与配置同步", "Advanced & Config Sync"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.select_tab(NavTab::Launchers, cx);
                                    })),
                            ),
                    ),
            )
            .child(
                editorial_card(p)
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(section_header(
                                p,
                                tr(lang, "运行态路径", "RUNTIME TOPOLOGY"),
                                tr(lang, "本地运行态路径摘要", "Local Runtime Topology"),
                                None,
                            ))
                            .child(
                                Button::new("ov-open-state-dir")
                                    .xsmall()
                                    .outline()
                                    .icon(IconName::FolderOpen)
                                    .label(tr(lang, "打开状态目录", "Open Folder"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.action_open_state_dir(cx);
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_col()
                            .child(kv_row(p, tr(lang, "Web 面板", "Dashboard"), dashboard_url))
                            .child(kv_row(p, tr(lang, "agy 路径", "agy Binary"), agy_bin_str))
                            .child(kv_row(p, tr(lang, "上游代理", "Upstream Proxy"), proxy_str))
                            .child(kv_row(p, tr(lang, "状态目录", "State Dir"), state_dir_str))
                            .child(kv_row(p, tr(lang, "日志文件", "Gateway Log"), log_path_str)),
                    ),
            );

        // --- Bottom Recent Log Tail Preview ---
        let log_preview_text = if self.log_tail.trim().is_empty() {
            tr(
                lang,
                "暂无网关日志输出（启动网关后此处自动滚动显示最新日志）。",
                "No gateway log lines yet (start the gateway to stream live logs here).",
            )
            .to_owned()
        } else {
            let tail_lines: Vec<&str> = self.log_tail.lines().rev().take(12).collect();
            tail_lines.into_iter().rev().collect::<Vec<_>>().join("\n")
        };

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_4()
            .child(hero_card)
            .child(kpi_row)
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap_4()
                    .child(form_card)
                    .child(right_col),
            )
            .child(
                editorial_card(p)
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(p.text)
                                    .child(tr(lang, "最新网关日志摘要", "Recent Gateway Log Tail")),
                            )
                            .child(
                                Button::new("ov-goto-logs")
                                    .xsmall()
                                    .ghost()
                                    .icon(IconName::ArrowRight)
                                    .label(tr(lang, "查看完整日志与环境诊断", "Full Logs & Doctor"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.select_tab(NavTab::System, cx);
                                    })),
                            ),
                    )
                    .child(sf_terminal_box(
                        p,
                        "overview-log-tail",
                        tr(
                            lang,
                            "gateway.log（最近 12 行）",
                            "gateway.log (latest 12 lines)",
                        ),
                        &log_preview_text,
                        px(180.0),
                    )),
            )
    }
}

fn labeled_input(p: &Palette, label: impl Into<SharedString>, input: impl IntoElement) -> Div {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(p.text_2)
                .child(label.into()),
        )
        .child(input)
}
