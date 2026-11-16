//! Tab 06: Live Logs, Environment Doctor, CLI Alias, Completions & Self-Upgrade (`日志 · 诊断 · 系统`).

use clap_complete::Shell;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{IconName, Sizable as _};
use gpui_kit::*;
use liftgate_core::BUILD_TARGET;
use liftgate_core::format::{format_bytes, format_utc_hms};

use crate::app::{DesktopApp, render_completions_text};
use crate::i18n::tr;
use crate::theme::{
    BadgeTone, Palette, editorial_card, kv_row, pill_badge, section_header, sf_terminal_box,
};

impl DesktopApp {
    pub(crate) fn render_system_tab(&self, p: &Palette, cx: &mut Context<Self>) -> Div {
        let lang = self.lang;

        // --- 1. Gateway Log Viewer & `agy acp` Stderr Diagnostics ---
        let log_q = self
            .in_log_filter
            .read(cx)
            .value()
            .trim()
            .to_ascii_lowercase();
        let filtered_log = if self.log_tail.trim().is_empty() {
            tr(
                lang,
                "日志文件当前为空。",
                "Gateway log file is currently empty.",
            )
            .to_owned()
        } else if log_q.is_empty() {
            self.log_tail.clone()
        } else {
            let matched: Vec<&str> = self
                .log_tail
                .lines()
                .filter(|line| line.to_ascii_lowercase().contains(&log_q))
                .collect();
            if matched.is_empty() {
                tr(
                    lang,
                    "没有匹配当前关键词的日志行。",
                    "No log lines match the filter.",
                )
                .to_owned()
            } else {
                matched.join("\n")
            }
        };
        let log_for_copy = filtered_log.clone();

        let stderr_lines: Option<String> = self
            .live_telemetry
            .as_ref()
            .or_else(|| {
                self.gateway_report
                    .as_ref()
                    .and_then(|r| r.telemetry.as_ref())
            })
            .map(|t| &t.acp_process.recent_stderr)
            .filter(|entries| !entries.is_empty())
            .map(|entries| {
                entries
                    .iter()
                    .map(|e| format!("[{}] {}", format_utc_hms(e.timestamp_unix_ms), e.line))
                    .collect::<Vec<_>>()
                    .join("\n")
            });

        let logs_card = editorial_card(p)
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(section_header(
                        p,
                        "DAEMON LOGS & ACP STDERR",
                        tr(
                            lang,
                            "网关实时日志查看器 (`lg logs`)",
                            "Gateway Log Tail & ACP Diagnostics (`lg logs`)",
                        ),
                        None,
                    ))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Switch::new("sys-sw-logfollow")
                                    .small()
                                    .checked(self.log_auto_follow)
                                    .label(tr(lang, "实时跟随 (--follow)", "Auto-Follow"))
                                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                        this.log_auto_follow = *checked;
                                        cx.notify();
                                    })),
                            )
                            .children([100_usize, 250, 500, 1000].into_iter().map(|n| {
                                let active = self.log_lines_limit == n;
                                let b = Button::new(SharedString::from(format!("log-lim-{n}")))
                                    .xsmall()
                                    .label(format!("{n}"));
                                let styled = if active { b.primary() } else { b.outline() };
                                styled.on_click(cx.listener(move |this, _, _, cx| {
                                    this.log_lines_limit = n;
                                    this.refresh_logs(cx);
                                }))
                            })),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(220.0))
                            .child(Input::new(&self.in_log_filter).small().cleanable(true)),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .child(
                                Button::new("sys-log-refresh")
                                    .small()
                                    .outline()
                                    .icon(IconName::RefreshCw)
                                    .label(tr(lang, "刷新", "Refresh"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.refresh_logs(cx);
                                    })),
                            )
                            .child(
                                Button::new("sys-log-copy")
                                    .small()
                                    .ghost()
                                    .icon(IconName::Copy)
                                    .label(tr(lang, "复制", "Copy"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.copy_text("Gateway Logs", log_for_copy.clone(), cx);
                                    })),
                            )
                            .child(
                                Button::new("sys-log-open-dir")
                                    .small()
                                    .outline()
                                    .icon(IconName::FolderOpen)
                                    .label(tr(lang, "日志目录", "Open Folder"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.action_open_state_dir(cx);
                                    })),
                            )
                            .child(
                                Button::new("sys-log-clear")
                                    .small()
                                    .danger()
                                    .icon(IconName::Delete)
                                    .label(tr(lang, "清空 (--clear)", "Clear"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.action_clear_logs(cx);
                                    })),
                            ),
                    ),
            )
            .child(sf_terminal_box(
                p,
                "system-gateway-log-box",
                self.state_paths.log_file().display().to_string(),
                &filtered_log,
                px(270.0),
            ))
            .children(stderr_lines.map(|stderr_txt| {
                sf_terminal_box(
                    p,
                    "system-acp-stderr-box",
                    "agy acp recent_stderr ring buffer",
                    &stderr_txt,
                    px(150.0),
                )
            }));

        // --- 2. Environment Doctor (`lg doctor`) ---
        let doc = self.doctor_report.as_ref();
        let doctor_card = editorial_card(p)
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(section_header(
                        p,
                        "ENVIRONMENT DIAGNOSTICS",
                        tr(lang, "全链路环境健康检查 (`lg doctor`)", "End-to-End Environment Doctor (`lg doctor`)"),
                        Some(tr(
                            lang,
                            "逐项核验 `agy` 二进制、Google OAuth 登录态、本地网关健康度、代理连通性与 AI 客户端可用性。",
                            "Verifies `agy` binary, Google OAuth session, local gateway health, upstream proxy, and client CLIs.",
                        )),
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .children(doc.map(|d| {
                                if d.ok && !d.warnings {
                                    pill_badge(p, BadgeTone::Ok, "ALL CHECKS PASSED")
                                } else if d.ok {
                                    pill_badge(p, BadgeTone::Warn, "PASSED WITH WARNINGS")
                                } else {
                                    pill_badge(p, BadgeTone::Err, "ACTION REQUIRED")
                                }
                            }))
                            .children(doc.filter(|d| d.agy_missing).map(|_| {
                                Button::new("doc-fix-agy")
                                    .small()
                                    .outline()
                                    .icon(IconName::ArrowDown)
                                    .label(tr(lang, "一键安装 agy", "Install agy"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.action_agy_install_or_update(false, false, cx);
                                    }))
                            }))
                            .children(doc.filter(|d| d.login_needed).map(|_| {
                                Button::new("doc-fix-login")
                                    .small()
                                    .outline()
                                    .icon(IconName::User)
                                    .label(tr(lang, "一键登录 agy", "Sign In agy"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.action_agy_login(cx);
                                    }))
                            }))
                            .child(
                                Button::new("doc-run-btn")
                                    .small()
                                    .primary()
                                    .loading(self.busy_doctor)
                                    .icon(IconName::Inspector)
                                    .label(tr(lang, "执行全量诊断", "Run Doctor"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.action_run_doctor(cx);
                                    })),
                            ),
                    ),
            )
            .child(if let Some(d) = doc {
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .children(d.checks.iter().map(|chk| {
                        let tone = BadgeTone::from_report(chk.tone);
                        div()
                            .w_full()
                            .px_3()
                            .py_2()
                            .rounded(px(8.0))
                            .bg(p.surface_alt)
                            .border_1()
                            .border_color(p.border)
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2p5()
                                    .child(pill_badge(p, tone, chk.status))
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(p.text)
                                            .child(chk.name),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .text_xs()
                                    .text_right()
                                    .text_color(p.text_2)
                                    .child(chk.detail.clone()),
                            )
                    }))
            } else {
                div()
                    .py_3()
                    .text_xs()
                    .text_color(p.muted)
                    .child(tr(
                        lang,
                        "点击「执行全量诊断」开始检查运行环境。",
                        "Click 'Run Doctor' to inspect your local environment.",
                    ))
            });

        // --- 3A. CLI Short Alias (`lg alias`) & Shell Completions (`lg completions`) ---
        let alias_rep = self.alias_report.as_ref();
        let comp_for_copy = self.completions_preview.clone();

        let alias_comp_card = editorial_card(p)
            .flex_1()
            .child(section_header(
                p,
                "CLI ALIAS & SHELL COMPLETIONS",
                tr(
                    lang,
                    "快捷别名 (`lg alias`) 与补全脚本",
                    "Short CLI Alias & Shell Completions",
                ),
                None,
            ))
            .children(alias_rep.map(|ar| {
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
                        "Target Alias Path",
                        ar.target_path.display().to_string(),
                    ))
                    .child(kv_row(
                        p,
                        "Status",
                        format!("installed={} · in_path={}", ar.installed, ar.dir_in_path),
                    ))
            }))
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.in_alias_name).small()),
                    )
                    .child(
                        Button::new("alias-install-btn")
                            .small()
                            .primary()
                            .icon(IconName::Check)
                            .label(tr(lang, "安装别名", "Install Alias"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.action_manage_alias(false, cx);
                            })),
                    )
                    .child(
                        Button::new("alias-remove-btn")
                            .small()
                            .outline()
                            .icon(IconName::Delete)
                            .label(tr(lang, "移除", "Remove"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.action_manage_alias(true, cx);
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
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div().flex().flex_wrap().items_center().gap_1p5().children(
                            [
                                (Shell::PowerShell, "PowerShell"),
                                (Shell::Bash, "Bash"),
                                (Shell::Zsh, "Zsh"),
                                (Shell::Fish, "Fish"),
                                (Shell::Elvish, "Elvish"),
                            ]
                            .into_iter()
                            .map(|(sh, lbl)| {
                                let sel = self.selected_shell == sh;
                                let b = Button::new(SharedString::from(format!("sh-{lbl}")))
                                    .xsmall()
                                    .label(lbl);
                                let styled = if sel { b.primary() } else { b.outline() };
                                styled.on_click(cx.listener(move |this, _, _, cx| {
                                    this.selected_shell = sh;
                                    this.completions_preview =
                                        render_completions_text(sh, "liftgate");
                                    cx.notify();
                                }))
                            }),
                        ),
                    )
                    .child(
                        Button::new("copy-completions-btn")
                            .xsmall()
                            .ghost()
                            .icon(IconName::Copy)
                            .label(tr(lang, "复制补全脚本", "Copy Script"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.copy_text("Completions Script", comp_for_copy.clone(), cx);
                            })),
                    ),
            )
            .child(sf_terminal_box(
                p,
                "completions-preview-box",
                format!("liftgate completions {:?}", self.selected_shell),
                &self.completions_preview,
                px(160.0),
            ));

        // --- 3B. Atomic Verified Self-Upgrade (`lg upgrade`) ---
        let upg = self.upgrade_outcome.as_ref();
        let upgrade_card = editorial_card(p)
            .flex_1()
            .child(section_header(
                p,
                "ATOMIC VERIFIED SELF-UPGRADE",
                tr(lang, "GitHub Release 自升级 (`lg upgrade`)", "Verified Release Self-Upgrade (`lg upgrade`)"),
                Some(tr(
                    lang,
                    "校验官方 `SHA256SUMS` 签名摘要，原子替换 `liftgate`、`lg` 与 `liftgate-desktop` 并平滑重启守护进程。",
                    "Verifies official `SHA256SUMS`, atomically updates `liftgate`, `lg`, and `liftgate-desktop`, and restarts the daemon.",
                )),
            ))
            .child(
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
                        tr(lang, "当前版本", "Current Version"),
                        format!("v{}", env!("CARGO_PKG_VERSION")),
                    ))
                    .child(kv_row(p, "Target Triple", BUILD_TARGET))
                    .children(upg.map(|u| {
                        kv_row(
                            p,
                            tr(lang, "远端版本状态", "Release Status"),
                            format!(
                                "{} (target v{} · {}{})",
                                u.status,
                                u.target_version,
                                u.archive_name,
                                u.archive_size_bytes
                                    .map_or_else(String::new, |b| format!(" · {}", format_bytes(b)))
                            ),
                        )
                    }))
                    .children(upg.and_then(|u| u.sha256.as_ref()).map(|sha| {
                        kv_row(p, "SHA-256", sha.clone())
                    })),
            )
            .child(Input::new(&self.in_upgrade_version).small().cleanable(true))
            .child(
                Switch::new("upg-sw-force")
                    .small()
                    .checked(self.upgrade_force)
                    .label(tr(
                        lang,
                        "版本相同时仍强制覆盖重装 (--force)",
                        "Force Reinstall Even If Up-To-Date (--force)",
                    ))
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.upgrade_force = *checked;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .w_full()
                    .pt_1()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("upg-check-btn")
                            .small()
                            .outline()
                            .loading(self.busy_upgrade)
                            .icon(IconName::RefreshCw)
                            .label(tr(lang, "检查最新版本 (--check)", "Check for Updates"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.action_run_upgrade(true, cx);
                            })),
                    )
                    .child(
                        Button::new("upg-exec-btn")
                            .small()
                            .primary()
                            .loading(self.busy_upgrade)
                            .icon(IconName::ArrowDown)
                            .label(tr(lang, "下载校验并执行升级", "Download & Atomic Upgrade"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.action_run_upgrade(false, cx);
                            })),
                    ),
            );

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_4()
            .child(logs_card)
            .child(doctor_card)
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap_4()
                    .child(alias_comp_card)
                    .child(upgrade_card),
            )
    }
}
