//! Tab 05: Antigravity (`agy`) Lifecycle, Google Auth & Proxy Manager (`AGY 与网络代理`).

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{IconName, Sizable as _};
use gpui_kit::*;
use liftgate_core::agy_manager::{self, AgyAuthStatus};

use crate::app::{DesktopApp, ProxyApplyMode};
use crate::i18n::tr;
use crate::theme::{
    BadgeTone, Palette, editorial_card, kv_row, pill_badge, section_header, sf_terminal_box,
};

impl DesktopApp {
    pub(crate) fn render_agy_proxy_tab(
        &self,
        p: &Palette,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let lang = self.lang;
        let agy = self.agy_report.as_ref();

        let (auth_badge_lbl, auth_tone, auth_detail) = match agy.map(|r| &r.auth_status) {
            Some(AgyAuthStatus::SignedIn {
                model_count,
                source,
            }) => (
                tr(lang, "已登录 GOOGLE 账号", "GOOGLE ACCOUNT SIGNED IN"),
                BadgeTone::Ok,
                if lang.is_en() {
                    format!("{model_count} models verified via {source}")
                } else {
                    format!("已验证 {model_count} 个可用模型（来源: {source}）")
                },
            ),
            Some(AgyAuthStatus::NotSignedIn { detail }) => (
                tr(lang, "需要登录授权", "SIGN-IN REQUIRED"),
                BadgeTone::Warn,
                detail.clone(),
            ),
            Some(AgyAuthStatus::NetworkError { detail }) => (
                tr(lang, "鉴权探测网络异常", "AUTH PROBE NETWORK ERROR"),
                BadgeTone::Err,
                detail.clone(),
            ),
            Some(AgyAuthStatus::NotInstalled) => (
                tr(lang, "尚未安装 AGY", "AGY NOT INSTALLED"),
                BadgeTone::Err,
                tr(
                    lang,
                    "尚未安装官方 `agy` CLI",
                    "Official `agy` CLI is not installed",
                )
                .to_owned(),
            ),
            _ => (
                tr(lang, "尚未校验", "UNVERIFIED"),
                BadgeTone::Neutral,
                tr(
                    lang,
                    "点击「校验登录与版本」执行实况探测",
                    "Click 'Verify Auth & Version' to probe live status",
                )
                .to_owned(),
            ),
        };

        let bin_str = agy
            .and_then(|r| r.binary_path.as_ref().map(|b| b.display().to_string()))
            .unwrap_or_else(|| "—".to_owned());
        let ver_str = agy
            .and_then(|r| r.version.clone())
            .unwrap_or_else(|| "—".to_owned());
        let latest_str = agy
            .and_then(|r| r.latest_version.clone())
            .unwrap_or_else(|| "—".to_owned());
        let auto_upd = agy.map_or(self.cfg_agy_auto_update, |r| r.auto_update_enabled);

        // --- 1. Antigravity (`agy`) Lifecycle & Auth Card ---
        let agy_card = editorial_card(p)
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
                        tr(lang, "官方 ANTIGRAVITY ACP 引擎", "OFFICIAL ANTIGRAVITY ACP ENGINE"),
                        tr(lang, "Antigravity (`agy`) 安装、升级与账号认证", "Antigravity (`agy`) Lifecycle & Google OAuth"),
                        Some(tr(
                            lang,
                            "严格通过官方 `agy` CLI 完备管理安装、空闲热更新与浏览器 OAuth 登录。",
                            "Manages official `agy` installation, idle auto-updates, and browser Google OAuth sign-in.",
                        )),
                    ))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(pill_badge(p, auth_tone, auth_badge_lbl))
                            .children(agy.is_some_and(|r| r.update_available).then(|| {
                                pill_badge(
                                    p,
                                    BadgeTone::Brand,
                                    if lang.is_en() {
                                        format!("UPDATE AVAILABLE -> {latest_str}")
                                    } else {
                                        format!("发现新版本 -> {latest_str}")
                                    },
                                )
                            })),
                    ),
            )
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
                    .child(kv_row(p, tr(lang, "二进制路径", "Binary Path"), bin_str))
                    .child(kv_row(p, tr(lang, "当前已安装版本", "Installed Version"), ver_str))
                    .child(kv_row(p, tr(lang, "上游最新稳定版", "Latest Upstream"), latest_str))
                    .child(kv_row(p, tr(lang, "账号鉴权详情", "Auth Status"), auth_detail)),
            )
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
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("agy-btn-login")
                                    .small()
                                    .primary()
                                    .loading(self.busy_agy)
                                    .icon(IconName::User)
                                    .label(tr(lang, "登录 Google 账号 (`agy login`)", "Sign In (`agy login`)"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.action_agy_login(cx);
                                    })),
                            )
                            .child(
                                Button::new("agy-btn-verify")
                                    .small()
                                    .secondary()
                                    .loading(self.busy_agy)
                                    .icon(IconName::CircleCheck)
                                    .label(tr(lang, "校验登录与版本", "Verify Auth & Version"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.action_refresh_agy_status(true, cx);
                                    })),
                            )
                            .child(
                                Button::new("agy-btn-check-upd")
                                    .small()
                                    .outline()
                                    .loading(self.busy_agy)
                                    .icon(IconName::RefreshCw)
                                    .label(tr(lang, "仅检查更新", "Check Update"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.action_agy_install_or_update(true, false, cx);
                                    })),
                            )
                            .child(
                                Button::new("agy-btn-install-upd")
                                    .small()
                                    .outline()
                                    .loading(self.busy_agy)
                                    .icon(IconName::ArrowDown)
                                    .label(tr(lang, "安装 / 升级 agy", "Install / Update agy"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.action_agy_install_or_update(false, false, cx);
                                    })),
                            )
                            .child(
                                Button::new("agy-btn-force")
                                    .small()
                                    .ghost()
                                    .loading(self.busy_agy)
                                    .label(tr(lang, "强制重装", "Force Reinstall"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.action_agy_install_or_update(false, true, cx);
                                    })),
                            ),
                    )
                    .child(
                        Switch::new("agy-sw-autoupd")
                            .small()
                            .checked(auto_upd)
                            .label(tr(lang, "后台空闲自动热更新", "Idle Auto-Update"))
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.action_toggle_agy_auto_update(*checked, cx);
                            })),
                    ),
            );

        // --- 2. Upstream Proxy Manager (`lg proxy`) ---
        let px_out = self.proxy_outcome.as_ref();
        let configured_px = px_out
            .and_then(|o| o.configured_proxy.as_ref().map(|u| u.as_str().to_owned()))
            .unwrap_or_else(|| {
                tr(lang, "直连（未保存强制代理）", "direct (none saved)").to_owned()
            });
        let effective_px = px_out
            .and_then(|o| o.effective_proxy.as_ref().map(|u| u.as_str().to_owned()))
            .or_else(|| agy.and_then(|r| r.effective_proxy.as_ref().map(|u| u.as_str().to_owned())))
            .unwrap_or_else(|| tr(lang, "直连（未启用代理）", "direct (off)").to_owned());
        let ambient_px = px_out
            .and_then(|o| o.ambient_system_proxy.clone())
            .or_else(agy_manager::detect_ambient_system_proxy)
            .unwrap_or_else(|| {
                tr(
                    lang,
                    "未检测到系统环境变量或 OS 代理",
                    "None detected in OS / env",
                )
                .to_owned()
            });

        let probe_summary = match px_out.and_then(|o| o.connectivity_probe.as_ref()) {
            Some(pr) if pr.ok => {
                let latest_v = pr.latest_agy_version.as_deref().unwrap_or("unknown");
                if lang.is_en() {
                    format!("OK · {} ms (upstream manifest v{latest_v})", pr.latency_ms)
                } else {
                    format!("正常 · {} ms（上游清单 v{latest_v}）", pr.latency_ms)
                }
            }
            Some(pr) => {
                let err_s = pr.error.as_deref().unwrap_or("unreachable");
                if lang.is_en() {
                    format!("FAILED ({} ms): {err_s}", pr.latency_ms)
                } else {
                    format!("失败（{} ms）：{err_s}", pr.latency_ms)
                }
            }
            None => tr(
                lang,
                "点击「连通性测速」验证上游可达性与延迟",
                "Click 'Test Upstream Latency' to measure RTT",
            )
            .to_owned(),
        };

        let proxy_card = editorial_card(p)
            .child(section_header(
                p,
                tr(lang, "上游 HTTP / SOCKS5 代理", "UPSTREAM HTTP / SOCKS5 PROXY"),
                tr(lang, "上游网络代理管理 (`lg proxy`)", "Upstream Proxy Configuration (`lg proxy`)"),
                Some(tr(
                    lang,
                    "支持快捷填写本机端口号（如 `7890` 自动展开为 `http://127.0.0.1:7890`）、完整 HTTP 或 SOCKS5 地址；变更后自动热重启托管网关。",
                    "Accepts bare local port (`7890`), `http://`, or `socks5://` URLs; automatically restarts managed daemon on change.",
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
                    .child(kv_row(p, tr(lang, "持久化配置代理", "Configured Proxy"), configured_px))
                    .child(kv_row(p, tr(lang, "当前生效代理", "Effective Proxy"), effective_px))
                    .child(kv_row(p, tr(lang, "系统环境代理", "System / Env Proxy"), ambient_px))
                    .child(kv_row(p, tr(lang, "上游测速结果", "Connectivity Probe"), probe_summary)),
            )
            .child(Input::new(&self.in_proxy_target).small().cleanable(true))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("px-btn-apply")
                            .small()
                            .primary()
                            .loading(self.busy_proxy)
                            .icon(IconName::Check)
                            .label(tr(lang, "保存并应用代理", "Save & Apply"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.action_apply_proxy(ProxyApplyMode::ApplyInput, cx);
                            })),
                    )
                    .child(
                        Button::new("px-btn-system")
                            .small()
                            .secondary()
                            .loading(self.busy_proxy)
                            .icon(IconName::Globe)
                            .label(tr(lang, "导入系统代理 (--system)", "Use System Proxy"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.action_apply_proxy(ProxyApplyMode::UseSystem, cx);
                            })),
                    )
                    .child(
                        Button::new("px-btn-off")
                            .small()
                            .outline()
                            .loading(self.busy_proxy)
                            .icon(IconName::Ban)
                            .label(tr(lang, "关闭代理直连 (off)", "Disable (Direct)"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.action_apply_proxy(ProxyApplyMode::DisableDirect, cx);
                            })),
                    )
                    .child(
                        Button::new("px-btn-test")
                            .small()
                            .outline()
                            .loading(self.busy_proxy)
                            .icon(IconName::ChartPie)
                            .label(tr(lang, "连通性测速 (--test)", "Test Upstream Latency"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.action_apply_proxy(ProxyApplyMode::TestLatency, cx);
                            })),
                    ),
            );

        // --- 3. Direct `agy` Subcommand Runner ---
        let presets: &[(&str, &[&str])] = &[
            ("--version", &["--version"]),
            ("acp --help", &["acp", "--help"]),
            ("mcp --help", &["mcp", "--help"]),
            ("plugin --help", &["plugin", "--help"]),
            ("--help", &["--help"]),
        ];

        let runner_card = editorial_card(p)
            .child(section_header(
                p,
                tr(lang, "原生 AGY 命令运行器", "PASSTHROUGH AGY RUNNER"),
                tr(lang, "原生 `agy` 命令运行器 (`lg agy <args>`)", "Direct `agy` Command Runner (`lg agy <args>`)"),
                Some(tr(
                    lang,
                    "自动注入托管 `agy` 路径与生效代理环境变量，支持捕获标准输出或在交互式终端打开。",
                    "Automatically injects managed `agy` binary path and active proxy; capture output inline or open in terminal.",
                )),
            ))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1p5()
                    .children(presets.iter().enumerate().map(|(idx, (label, args))| {
                        let preset_vec: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
                        let preset_str = args.join(" ");
                        Button::new(SharedString::from(format!("agy-pre-{idx}")))
                            .xsmall()
                            .outline()
                            .label(*label)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let text_val = preset_str.clone();
                                this.in_agy_custom_args
                                    .update(cx, |s, cx| s.set_value(text_val, window, cx));
                                this.action_run_agy_subcommand(Some(preset_vec.clone()), false, cx);
                            }))
                    })),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.in_agy_custom_args).small().cleanable(true)),
                    )
                    .child(
                        Button::new("agy-run-cap")
                            .small()
                            .primary()
                            .loading(self.busy_agy)
                            .icon(IconName::Play)
                            .label(tr(lang, "运行并捕获输出", "Run & Capture"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.action_run_agy_subcommand(None, false, cx);
                            })),
                    )
                    .child(
                        Button::new("agy-run-term")
                            .small()
                            .outline()
                            .icon(IconName::SquareTerminal)
                            .label(tr(lang, "在系统终端运行", "Run in Terminal"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.action_run_agy_subcommand(None, true, cx);
                            })),
                    ),
            )
            .child(sf_terminal_box(
                p,
                "agy-runner-output",
                "agy stdout / stderr",
                if self.agy_terminal_output.is_empty() {
                    tr(
                        lang,
                        "选择上方快捷命令或输入任意参数点击「运行并捕获输出」即可在此查看结果。",
                        "Click a preset above or enter custom arguments to inspect `agy` output here.",
                    )
                } else {
                    &self.agy_terminal_output
                },
                px(220.0),
            ));

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_4()
            .child(agy_card)
            .child(proxy_card)
            .child(runner_card)
    }
}
