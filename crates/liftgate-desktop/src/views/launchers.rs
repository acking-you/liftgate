//! Tab 03: Zero-Pollution AI Harness Launchers & Persistent Config Sync (`启动器与配置同步`).

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{IconName, Sizable as _};
use gpui_kit::*;
use liftgate_core::cli::ConfigTarget;

use crate::app::{DesktopApp, ToolBridgeMode};
use crate::i18n::tr;
use crate::theme::{
    BadgeTone, Palette, editorial_card, kv_row, pill_badge, section_header, sf_terminal_box,
};

impl DesktopApp {
    pub(crate) fn render_launchers_tab(&self, p: &Palette, cx: &mut Context<Self>) -> Div {
        let lang = self.lang;

        // --- 1. Shared Session Scope & Permissions Card ---
        let scope_card = editorial_card(p)
            .child(section_header(
                p,
                "SESSION WORKSPACE & OVERRIDES",
                tr(lang, "启动工作目录与模型覆盖", "Target Workspace & Model Override"),
                Some(tr(
                    lang,
                    "以下目录与模型参数同时作用于 Claude Code CLI、Codex CLI、桌面客户端以及项目级配置同步。",
                    "Shared across Claude Code CLI, Codex CLI, desktop launchers, and project-scoped config sync.",
                )),
            ))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .gap_3()
                    .child(field_box(
                        p,
                        tr(lang, "项目工作目录 (Working Directory)", "Project Working Directory"),
                        Input::new(&self.in_launch_dir).small().cleanable(true),
                    ))
                    .child(field_box(
                        p,
                        tr(
                            lang,
                            "指定模型覆盖 (--model，留空使用网关默认模型)",
                            "Model Override (--model, blank = gateway default)",
                        ),
                        Input::new(&self.in_launch_model).small().cleanable(true),
                    )),
            )
            .child(
                div()
                    .w_full()
                    .pt_1()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_5()
                    .child(
                        Switch::new("ln-sw-bypass")
                            .small()
                            .checked(self.launch_bypass_perms)
                            .label(tr(
                                lang,
                                "自动授权无确认模式 (--dangerously-skip-permissions / full-auto)",
                                "Bypass Interactive Permission Prompts",
                            ))
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.launch_bypass_perms = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        Switch::new("ln-sw-hold")
                            .small()
                            .checked(self.launch_hold_open)
                            .label(tr(
                                lang,
                                "CLI 退出后保留系统终端窗口",
                                "Keep System Terminal Open After Exit",
                            ))
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.launch_hold_open = *checked;
                                cx.notify();
                            })),
                    ),
            );

        // --- 2. CLI Terminal Launchers (Claude Code + Codex) ---
        let claude_cli_card = editorial_card(p)
            .flex_1()
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(section_header(
                        p,
                        "TERMINAL HARNESS · ANTHROPIC",
                        "Claude Code CLI (`lg claude`)",
                        Some(tr(
                            lang,
                            "注入进程级 ANTHROPIC_BASE_URL 与 1M 上下文策略，不修改 `~/.claude/settings.json`。",
                            "Injects process-scoped ANTHROPIC_BASE_URL & context limits without touching `~/.claude/settings.json`.",
                        )),
                    ))
                    .child(pill_badge(p, BadgeTone::Brand, "/v1/messages")),
            )
            .child(tool_mode_selector(
                p,
                "claude-tm",
                self.claude_tool_mode,
                lang,
                cx,
                true,
            ))
            .child(field_box(
                p,
                tr(
                    lang,
                    "透传参数 (Passthrough Args，如 `--continue` 或 `-p \"review\"`)",
                    "Passthrough Arguments (e.g. `--continue` or `-p \"review\"`)",
                ),
                Input::new(&self.in_claude_extra).small().cleanable(true),
            ))
            .child(
                div()
                    .w_full()
                    .pt_1()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("ln-claude-run")
                            .small()
                            .primary()
                            .loading(self.busy_launcher)
                            .icon(IconName::SquareTerminal)
                            .label(tr(lang, "在系统终端启动 Claude", "Launch in Terminal"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.action_launch_claude_cli(None, false, cx);
                            })),
                    )
                    .child(
                        Button::new("ln-claude-prev")
                            .small()
                            .outline()
                            .icon(IconName::Copy)
                            .label(tr(lang, "预览并复制启动命令", "Preview & Copy Command"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.action_launch_claude_cli(None, true, cx);
                            })),
                    ),
            );

        let codex_cli_card = editorial_card(p)
            .flex_1()
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(section_header(
                        p,
                        "TERMINAL HARNESS · OPENAI RESPONSES",
                        "Codex CLI (`lg codex`)",
                        Some(tr(
                            lang,
                            "通过 `-c` 临时注入 `model_catalog_json` 与本地网关 Provider，不污染 `~/.codex/config.toml`。",
                            "Passes `-c model_catalog_json=...` inline; never mutates `~/.codex/config.toml`.",
                        )),
                    ))
                    .child(pill_badge(p, BadgeTone::Info, "/v1/responses")),
            )
            .child(tool_mode_selector(
                p,
                "codex-tm",
                self.codex_tool_mode,
                lang,
                cx,
                false,
            ))
            .child(field_box(
                p,
                tr(
                    lang,
                    "透传参数 (Passthrough Args，如 `resume --last` 或 `exec \"cargo test\"`)",
                    "Passthrough Arguments (e.g. `resume --last` or `exec \"cargo test\"`)",
                ),
                Input::new(&self.in_codex_extra).small().cleanable(true),
            ))
            .child(
                div()
                    .w_full()
                    .pt_1()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("ln-codex-run")
                            .small()
                            .primary()
                            .loading(self.busy_launcher)
                            .icon(IconName::Cpu)
                            .label(tr(lang, "在系统终端启动 Codex", "Launch in Terminal"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.action_launch_codex_cli(None, false, cx);
                            })),
                    )
                    .child(
                        Button::new("ln-codex-prev")
                            .small()
                            .outline()
                            .icon(IconName::Copy)
                            .label(tr(lang, "预览并复制启动命令", "Preview & Copy Command"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.action_launch_codex_cli(None, true, cx);
                            })),
                    ),
            );

        // --- 3. Desktop GUI Launchers (Codex App + Claude Desktop) ---
        let codex_app_info = &self.desktop_apps.codex_app;
        let claude_app_info = &self.desktop_apps.claude_desktop;
        let journals = self.desktop_apps.active_journals;

        let desktop_card = editorial_card(p)
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(section_header(
                        p,
                        "ISOLATED DESKTOP GUI LAUNCHERS",
                        tr(lang, "桌面客户端无痕接管（退出自动回滚）", "Desktop GUI Launchers (Auto-Rollback)"),
                        Some(tr(
                            lang,
                            "由后台 `__desktop-watch` 守护进程跟踪桌面应用生命周期，退出后自动还原用户原始配置。",
                            "Detached `__desktop-watch` supervisor restores user configs automatically when the app exits.",
                        )),
                    ))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(pill_badge(
                                p,
                                if journals > 0 {
                                    BadgeTone::Ok
                                } else {
                                    BadgeTone::Neutral
                                },
                                format!("Active Watchers: {journals}"),
                            ))
                            .child(
                                Switch::new("ln-sw-restart-app")
                                    .small()
                                    .checked(self.desktop_restart_app)
                                    .label(tr(lang, "重启运行中实例 (--restart-app)", "Restart Running App"))
                                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                        this.desktop_restart_app = *checked;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Switch::new("ln-sw-ephemeral")
                                    .small()
                                    .checked(self.codex_app_ephemeral)
                                    .label(tr(lang, "临时隔离目录 (--ephemeral)", "Ephemeral Home"))
                                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                        this.codex_app_ephemeral = *checked;
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .gap_4()
                    .child(
                        div()
                            .flex_1()
                            .p_3p5()
                            .rounded(px(8.0))
                            .bg(p.surface_alt)
                            .border_1()
                            .border_color(p.border)
                            .flex()
                            .flex_col()
                            .gap_2p5()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(p.text)
                                            .child("Codex App (`lg codex-app`)"),
                                    )
                                    .child(if codex_app_info.installed {
                                        pill_badge(
                                            p,
                                            if codex_app_info.running_pids.is_empty() {
                                                BadgeTone::Ok
                                            } else {
                                                BadgeTone::Brand
                                            },
                                            if codex_app_info.running_pids.is_empty() {
                                                "INSTALLED".to_owned()
                                            } else {
                                                format!("RUNNING ({} PID)", codex_app_info.running_pids.len())
                                            },
                                        )
                                    } else {
                                        pill_badge(p, BadgeTone::Neutral, "NOT DETECTED")
                                    }),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(p.muted)
                                    .child(
                                        codex_app_info
                                            .display_label
                                            .clone()
                                            .unwrap_or_else(|| {
                                                tr(lang, "未检测到本地 Codex App 安装路径", "Codex App binary not found in standard paths").to_owned()
                                            }),
                                    ),
                            )
                            .child(
                                div()
                                    .pt_1()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        Button::new("ln-launch-codex-app")
                                            .small()
                                            .primary()
                                            .loading(self.busy_launcher)
                                            .icon(IconName::Play)
                                            .label(tr(lang, "启动隔离版 Codex App", "Launch Codex App"))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.action_launch_codex_app(None, cx);
                                            })),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .p_3p5()
                            .rounded(px(8.0))
                            .bg(p.surface_alt)
                            .border_1()
                            .border_color(p.border)
                            .flex()
                            .flex_col()
                            .gap_2p5()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(p.text)
                                            .child("Claude Desktop (`lg claude-desktop`)"),
                                    )
                                    .child(if claude_app_info.installed {
                                        pill_badge(
                                            p,
                                            if claude_app_info.running_pids.is_empty() {
                                                BadgeTone::Ok
                                            } else {
                                                BadgeTone::Brand
                                            },
                                            if claude_app_info.running_pids.is_empty() {
                                                "INSTALLED".to_owned()
                                            } else {
                                                format!("RUNNING ({} PID)", claude_app_info.running_pids.len())
                                            },
                                        )
                                    } else {
                                        pill_badge(p, BadgeTone::Neutral, "NOT DETECTED")
                                    }),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(p.muted)
                                    .child(
                                        claude_app_info
                                            .display_label
                                            .clone()
                                            .unwrap_or_else(|| {
                                                tr(lang, "未检测到本地 Claude Desktop 安装路径", "Claude Desktop binary not found in standard paths").to_owned()
                                            }),
                                    ),
                            )
                            .child(
                                div()
                                    .pt_1()
                                    .flex()
                                    .flex_wrap()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        Button::new("ln-launch-claude-dt")
                                            .small()
                                            .primary()
                                            .loading(self.busy_launcher)
                                            .icon(IconName::Play)
                                            .label(tr(lang, "启动 Claude Desktop", "Launch Claude Desktop"))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.action_launch_claude_desktop(false, None, cx);
                                            })),
                                    )
                                    .child(
                                        Button::new("ln-restore-claude-dt")
                                            .small()
                                            .outline()
                                            .icon(IconName::RefreshCw)
                                            .label(tr(lang, "立即恢复原配置 (--restore)", "Restore Original Config"))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.action_launch_claude_desktop(true, None, cx);
                                            })),
                                    ),
                            ),
                    ),
            );

        // --- 4. Persistent Config Sync (`lg config`) ---
        let insp = self.config_inspection.as_ref();
        let config_card = editorial_card(p)
            .child(section_header(
                p,
                "PERSISTENT HARNESS CONFIG SYNC",
                tr(lang, "持久化配置文件同步 (`lg config`)", "Persistent Config Sync (`lg config`)"),
                Some(tr(
                    lang,
                    "为需要直接运行原生 `claude` / `codex` 命令的场景写入受控配置，支持一键无损卸载还原。",
                    "Writes managed settings to Claude Code and/or Codex config files with atomic backup and `--uninstall` rollback.",
                )),
            ))
            .child(
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
                            .items_center()
                            .gap_1p5()
                            .children(
                                [
                                    (ConfigTarget::All, tr(lang, "全部 (Claude + Codex)", "All")),
                                    (ConfigTarget::Claude, "Claude Code"),
                                    (ConfigTarget::Codex, "Codex"),
                                ]
                                .into_iter()
                                .map(|(tgt, lbl)| {
                                    let sel = self.config_target == tgt;
                                    let b = Button::new(SharedString::from(format!("cfg-tgt-{tgt:?}")))
                                        .xsmall()
                                        .label(lbl);
                                    let styled = if sel { b.primary() } else { b.outline() };
                                    styled.on_click(cx.listener(move |this, _, _, cx| {
                                        this.config_target = tgt;
                                        cx.notify();
                                    }))
                                }),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_4()
                            .child(
                                Switch::new("cfg-sw-global")
                                    .small()
                                    .checked(self.config_global)
                                    .label(tr(lang, "全局用户级配置 (--global)", "Global Scope (~/.claude & ~/.codex)"))
                                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                        this.config_global = *checked;
                                        this.refresh_config_inspection(cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Switch::new("cfg-sw-offline")
                                    .small()
                                    .checked(self.config_offline)
                                    .label(tr(lang, "离线生成 (--offline)", "Offline Seed"))
                                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                        this.config_offline = *checked;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Switch::new("cfg-sw-nocodemode")
                                    .small()
                                    .checked(self.config_no_code_mode)
                                    .label(tr(lang, "禁用 Code Mode", "Disable Code Mode"))
                                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                        this.config_no_code_mode = *checked;
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .child(field_box(
                p,
                tr(
                    lang,
                    "Claude 子智能体快速模型覆盖 (--fast-model，可选)",
                    "Claude Fast / Subagent Model Override (--fast-model, optional)",
                ),
                Input::new(&self.in_config_fast_model).small().cleanable(true),
            ))
            .children(insp.map(|ci| {
                div()
                    .w_full()
                    .p_3()
                    .rounded(px(8.0))
                    .bg(p.surface_alt)
                    .border_1()
                    .border_color(p.border)
                    .flex()
                    .flex_col()
                    .child(kv_row(
                        p,
                        "Claude Settings",
                        format!(
                            "{} [{}]",
                            ci.claude_path.display(),
                            if ci.claude_managed {
                                "MANAGED BY LIFTGATE"
                            } else if ci.claude_exists {
                                "EXISTING USER FILE"
                            } else {
                                "NOT CREATED"
                            }
                        ),
                    ))
                    .child(kv_row(
                        p,
                        "Codex Config",
                        format!(
                            "{} [{}]",
                            ci.codex_path.display(),
                            if ci.codex_managed {
                                "MANAGED BY LIFTGATE"
                            } else if ci.codex_exists {
                                "EXISTING USER FILE"
                            } else {
                                "NOT CREATED"
                            }
                        ),
                    ))
            }))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("cfg-apply-btn")
                            .small()
                            .primary()
                            .loading(self.busy_config)
                            .icon(IconName::Check)
                            .label(tr(lang, "写入同步配置", "Sync Configuration"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.action_run_config_sync(false, false, cx);
                            })),
                    )
                    .child(
                        Button::new("cfg-dryrun-btn")
                            .small()
                            .outline()
                            .loading(self.busy_config)
                            .icon(IconName::Eye)
                            .label(tr(lang, "预览生成内容 (--print)", "Dry-Run Preview"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.action_run_config_sync(true, false, cx);
                            })),
                    )
                    .child(
                        Button::new("cfg-uninstall-btn")
                            .small()
                            .danger()
                            .loading(self.busy_config)
                            .icon(IconName::Delete)
                            .label(tr(lang, "移除托管配置 (--uninstall)", "Remove Managed Config"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.action_run_config_sync(false, true, cx);
                            })),
                    ),
            )
            .children((!self.config_preview_output.is_empty()).then(|| {
                sf_terminal_box(
                    p,
                    "config-sync-preview-box",
                    "lg config preview / summary",
                    &self.config_preview_output,
                    px(220.0),
                )
            }));

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_4()
            .child(scope_card)
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap_4()
                    .child(claude_cli_card)
                    .child(codex_cli_card),
            )
            .children((!self.last_launcher_output.is_empty()).then(|| {
                let copy_cmd = self.last_launcher_output.clone();
                editorial_card(p)
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(p.text)
                                    .child(tr(
                                        lang,
                                        "最近一次启动命令 / 会话摘要",
                                        "Latest Launch Command / Session Output",
                                    )),
                            )
                            .child(
                                Button::new("copy-last-launcher-out")
                                    .xsmall()
                                    .ghost()
                                    .icon(IconName::Copy)
                                    .label(tr(lang, "复制", "Copy"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.copy_text("Launch Command", copy_cmd.clone(), cx);
                                    })),
                            ),
                    )
                    .child(sf_terminal_box(
                        p,
                        "launcher-cmd-preview",
                        "terminal launch spec",
                        &self.last_launcher_output,
                        px(150.0),
                    ))
            }))
            .child(desktop_card)
            .child(config_card)
    }
}

fn field_box(p: &Palette, label: impl Into<SharedString>, input: impl IntoElement) -> Div {
    div()
        .flex_1()
        .w_full()
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

fn tool_mode_selector(
    p: &Palette,
    prefix: &'static str,
    current: ToolBridgeMode,
    lang: liftgate_core::state::DesktopLang,
    cx: &mut Context<DesktopApp>,
    is_claude: bool,
) -> Div {
    let modes = [
        ToolBridgeMode::CodeMode,
        ToolBridgeMode::CodeModeOnly,
        ToolBridgeMode::DirectTools,
    ];
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(p.text_2)
                .child(tr(
                    lang,
                    "工具桥接模式 (Tool Bridge Mode)",
                    "Tool Bridge Mode",
                )),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_1p5()
                .children(modes.into_iter().map(|m| {
                    let active = current == m;
                    let b = Button::new(SharedString::from(format!("{prefix}-{m:?}")))
                        .xsmall()
                        .label(m.label(lang));
                    let styled = if active { b.primary() } else { b.outline() };
                    styled.on_click(cx.listener(move |this, _, _, cx| {
                        if is_claude {
                            this.claude_tool_mode = m;
                        } else {
                            this.codex_tool_mode = m;
                        }
                        cx.notify();
                    }))
                })),
        )
}
