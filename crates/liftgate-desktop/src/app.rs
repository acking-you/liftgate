//! Central `DesktopApp` GPUI view entity, Tokio async task bridge, live SSE telemetry
//! subscriber, and Warm Editorial (`ackingliu.top`) window chrome.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::CommandFactory as _;
use clap_complete::Shell;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::InputState;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{Icon, IconName, Sizable as _};
use gpui_kit::*;
use liftgate_core::agy_manager::{self, AgyStatusReport, ProxyOutcome};
use liftgate_core::cli::{
    AgyAutoUpdateArgs, AgyInstallArgs, AgyLoginArgs, AgyStatusArgs, AgyUpdateArgs, AliasArgs,
    AutoUpdateToggle, ClaudeArgs, ClaudeDesktopArgs, Cli, CodexAppArgs, CodexArgs, ConfigArgs,
    ConfigTarget, DoctorArgs, ModelsArgs, ProxyArgs, RestartArgs, ServeArgs, StatusArgs, UiArgs,
    UpgradeArgs, normalize_optional_api_key,
};
use liftgate_core::client::GatewayClient;
use liftgate_core::config_sync::{self, ConfigSyncInspection};
use liftgate_core::daemon::{
    self, AliasStatusReport, DoctorReport, GatewayStatusMode, GatewayStatusReport, ModelsReport,
    open_browser,
};
use liftgate_core::desktop::{self, DesktopAppsStatus};
use liftgate_core::format::format_tokens;
use liftgate_core::harness;
use liftgate_core::state::{
    DesktopLang, DesktopLangPref, DesktopThemePref, DesktopUiPrefs, StatePaths,
    detect_system_language,
};
use liftgate_core::terminal::{prepare_reusable_launcher_command, spawn_in_system_terminal};
use liftgate_core::upgrade::{self, UpgradeOutcome};
use liftgate_types::telemetry::{GatewayTelemetrySnapshot, TaskProtocol};
use tokio::sync::{oneshot, watch};

use crate::i18n::{NavTab, tr, window_title};
use crate::theme::{BadgeTone, Palette, apply_staticflow_theme, is_appearance_dark, pill_badge};

/// Tool-bridge mode selector for Claude Code and Codex CLI launchers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolBridgeMode {
    CodeMode,
    CodeModeOnly,
    DirectTools,
}

impl ToolBridgeMode {
    #[must_use]
    pub const fn label(self, lang: DesktopLang) -> &'static str {
        match self {
            Self::CodeMode => tr(lang, "Code Mode（推荐）", "Code Mode"),
            Self::CodeModeOnly => tr(lang, "纯 Code Mode", "Code Mode Only"),
            Self::DirectTools => tr(lang, "原生工具直传", "Direct Tools"),
        }
    }
}

/// Protocol filter for Live Telemetry task history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskProtocolFilter {
    All,
    OpenAiResponses,
    AnthropicMessages,
}

impl TaskProtocolFilter {
    pub const ALL: [Self; 3] = [Self::All, Self::OpenAiResponses, Self::AnthropicMessages];

    #[must_use]
    pub const fn label(self, lang: DesktopLang) -> &'static str {
        match self {
            Self::All => tr(lang, "全部协议", "All"),
            Self::OpenAiResponses => "OpenAI /v1/responses",
            Self::AnthropicMessages => "Anthropic /v1/messages",
        }
    }

    #[must_use]
    pub const fn matches(self, protocol: TaskProtocol) -> bool {
        match self {
            Self::All => true,
            Self::OpenAiResponses => matches!(protocol, TaskProtocol::OpenaiResponses),
            Self::AnthropicMessages => matches!(protocol, TaskProtocol::AnthropicMessages),
        }
    }
}

/// Model family filter for the `/v1/models` explorer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelFamilyFilter {
    All,
    Gemini,
    Claude,
    Other,
}

impl ModelFamilyFilter {
    pub const ALL: [Self; 4] = [Self::All, Self::Gemini, Self::Claude, Self::Other];

    #[must_use]
    pub const fn label(self, lang: DesktopLang) -> &'static str {
        match self {
            Self::All => tr(lang, "全部模型", "All"),
            Self::Gemini => "Gemini",
            Self::Claude => "Claude",
            Self::Other => tr(lang, "GPT / 其他", "GPT / Other"),
        }
    }

    #[must_use]
    pub fn matches(self, model_id: &str) -> bool {
        let lower = model_id.to_ascii_lowercase();
        match self {
            Self::All => true,
            Self::Gemini => lower.contains("gemini"),
            Self::Claude => lower.contains("claude"),
            Self::Other => !lower.contains("gemini") && !lower.contains("claude"),
        }
    }
}

/// Proxy mutation action dispatched from the Proxy Manager card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProxyApplyMode {
    RefreshOnly,
    ApplyInput,
    UseSystem,
    DisableDirect,
    TestLatency,
}

/// Dismissible top feedback callout.
#[derive(Clone, Debug)]
pub struct StatusBanner {
    pub tone: BadgeTone,
    pub title: String,
    pub detail: String,
}

/// Main Liftgate GPUI desktop view.
pub struct DesktopApp {
    pub(crate) rt: tokio::runtime::Handle,
    pub(crate) state_paths: StatePaths,
    pub(crate) current_exe: PathBuf,
    pub(crate) lang_pref: DesktopLangPref,
    pub(crate) theme_pref: DesktopThemePref,
    pub(crate) lang: DesktopLang,
    pub(crate) dark_mode: bool,
    _appearance_sub: Subscription,
    pub(crate) active_tab: NavTab,
    pub(crate) banner: Option<StatusBanner>,

    // Async in-flight flags
    pub(crate) busy_gateway: bool,
    pub(crate) busy_launcher: bool,
    pub(crate) busy_config: bool,
    pub(crate) busy_models: bool,
    pub(crate) busy_agy: bool,
    pub(crate) busy_proxy: bool,
    pub(crate) busy_doctor: bool,
    pub(crate) busy_upgrade: bool,

    // Gateway & SSE Live Telemetry
    pub(crate) gateway_report: Option<GatewayStatusReport>,
    pub(crate) live_telemetry: Option<GatewayTelemetrySnapshot>,
    pub(crate) sse_connected: bool,
    pub(crate) sse_target_key: Option<(SocketAddr, Option<String>)>,
    pub(crate) sse_cancel_tx: Option<watch::Sender<bool>>,
    pub(crate) gateway_form_hydrated: bool,

    // Gateway Serve Form Inputs
    pub(crate) in_listen: Entity<InputState>,
    pub(crate) in_ui_listen: Entity<InputState>,
    pub(crate) in_default_model: Entity<InputState>,
    pub(crate) in_acp_mode: Entity<InputState>,
    pub(crate) in_api_key: Entity<InputState>,
    pub(crate) in_max_idle_sessions: Entity<InputState>,
    pub(crate) in_turn_timeout_secs: Entity<InputState>,
    pub(crate) cfg_long_context: bool,
    pub(crate) cfg_agy_auto_update: bool,

    // Live Telemetry Inspector
    pub(crate) in_task_filter: Entity<InputState>,
    pub(crate) protocol_filter: TaskProtocolFilter,
    pub(crate) selected_task_id: Option<String>,

    // AI Launchers & Config Sync
    pub(crate) in_launch_dir: Entity<InputState>,
    pub(crate) in_launch_model: Entity<InputState>,
    pub(crate) in_claude_extra: Entity<InputState>,
    pub(crate) in_codex_extra: Entity<InputState>,
    pub(crate) claude_tool_mode: ToolBridgeMode,
    pub(crate) codex_tool_mode: ToolBridgeMode,
    pub(crate) launch_bypass_perms: bool,
    pub(crate) launch_hold_open: bool,
    pub(crate) desktop_apps: DesktopAppsStatus,
    pub(crate) desktop_restart_app: bool,
    pub(crate) codex_app_ephemeral: bool,
    pub(crate) last_launcher_output: String,

    pub(crate) config_target: ConfigTarget,
    pub(crate) config_global: bool,
    pub(crate) config_offline: bool,
    pub(crate) config_no_bypass: bool,
    pub(crate) config_no_code_mode: bool,
    pub(crate) in_config_fast_model: Entity<InputState>,
    pub(crate) config_inspection: Option<ConfigSyncInspection>,
    pub(crate) config_preview_output: String,

    // Models Catalog
    pub(crate) models_report: Option<ModelsReport>,
    pub(crate) in_model_search: Entity<InputState>,
    pub(crate) model_family: ModelFamilyFilter,

    // Antigravity & Proxy
    pub(crate) agy_report: Option<AgyStatusReport>,
    pub(crate) in_agy_custom_args: Entity<InputState>,
    pub(crate) agy_terminal_output: String,
    pub(crate) proxy_outcome: Option<ProxyOutcome>,
    pub(crate) in_proxy_target: Entity<InputState>,

    // Logs, Doctor & System
    pub(crate) log_tail: String,
    pub(crate) log_lines_limit: usize,
    pub(crate) log_auto_follow: bool,
    pub(crate) in_log_filter: Entity<InputState>,
    pub(crate) doctor_report: Option<DoctorReport>,
    pub(crate) alias_report: Option<AliasStatusReport>,
    pub(crate) in_alias_name: Entity<InputState>,
    pub(crate) selected_shell: Shell,
    pub(crate) completions_preview: String,
    pub(crate) upgrade_outcome: Option<UpgradeOutcome>,
    pub(crate) in_upgrade_version: Entity<InputState>,
    pub(crate) upgrade_force: bool,
}

impl DesktopApp {
    pub fn new(
        rt: tokio::runtime::Handle,
        state_paths: StatePaths,
        current_exe: PathBuf,
        prefs: DesktopUiPrefs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let defaults = ServeArgs::default();
        let settings = state_paths.read_settings().unwrap_or_default();
        let saved_daemon = state_paths.read_daemon().ok().flatten();

        let init_listen = saved_daemon
            .as_ref()
            .map_or(defaults.listen, |d| d.listen)
            .to_string();
        let init_ui_listen = saved_daemon
            .as_ref()
            .and_then(|d| d.ui_listen)
            .map(|a| a.to_string())
            .unwrap_or_default();
        let init_model = saved_daemon
            .as_ref()
            .map(|d| d.default_model.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| defaults.default_model.clone());
        let init_mode = saved_daemon
            .as_ref()
            .map(|d| d.acp_mode.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| defaults.acp_mode.clone());
        let init_key = saved_daemon
            .as_ref()
            .and_then(|d| d.api_key.clone())
            .or_else(|| defaults.api_key.clone())
            .unwrap_or_default();
        let init_long_ctx = saved_daemon
            .as_ref()
            .map_or(settings.long_context, |d| d.long_context);
        let init_auto_upd = saved_daemon
            .as_ref()
            .map_or(settings.agy_auto_update, |d| d.agy_auto_update);
        let init_idle_sessions = saved_daemon
            .as_ref()
            .map_or(defaults.max_idle_sessions, |d| d.max_idle_sessions);
        let init_turn_timeout = saved_daemon
            .as_ref()
            .map_or(defaults.turn_timeout_secs, |d| d.turn_timeout_secs);

        let default_dir = prefs
            .project_dir
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .or_else(|| liftgate_core::state::user_home_dir().ok())
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let init_proxy_str = settings
            .agy_proxy
            .as_ref()
            .map_or_else(String::new, |p| p.as_str().to_owned());

        let in_listen = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("127.0.0.1:8787")
                .default_value(init_listen)
        });
        let in_ui_listen = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("optional, e.g. 127.0.0.1:8788")
                .default_value(init_ui_listen)
        });
        let in_default_model = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("gemini-4-carbon-high")
                .default_value(init_model)
        });
        let in_acp_mode = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("agent")
                .default_value(init_mode)
        });
        let in_api_key = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("optional Bearer / x-api-key")
                .masked(true)
                .default_value(init_key)
        });
        let in_max_idle_sessions = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("16")
                .default_value(init_idle_sessions.to_string())
        });
        let in_turn_timeout_secs = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("1800")
                .default_value(init_turn_timeout.to_string())
        });

        let in_task_filter = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Search model, session, prompt or tool...")
                .clean_on_escape()
        });

        let in_launch_dir = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Project working directory")
                .default_value(default_dir)
        });
        let in_launch_model = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Optional model override (leave blank for gateway default)")
        });
        let in_claude_extra = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Optional claude args, e.g. --continue or -p \"review diff\"")
        });
        let in_codex_extra = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Optional codex args, e.g. resume --last or exec \"cargo test\"")
        });
        let in_config_fast_model = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Optional Claude fast/subagent model override")
        });

        let in_model_search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Filter by model ID or display name...")
                .clean_on_escape()
        });

        let in_agy_custom_args = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("models | mcp --help | plugin --help | --version")
                .default_value("--version")
        });
        let in_proxy_target = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("7890 | http://127.0.0.1:7890 | socks5://127.0.0.1:1080")
                .default_value(init_proxy_str)
        });

        let in_log_filter = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Filter log lines...")
                .clean_on_escape()
        });
        let in_alias_name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("lg")
                .default_value("lg")
        });
        let in_upgrade_version = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Optional tag (e.g. v0.1.8); empty = latest release")
        });

        let desktop_apps = desktop::inspect_desktop_apps(&state_paths);
        let default_shell = if cfg!(windows) {
            Shell::PowerShell
        } else {
            Shell::Zsh
        };
        let completions_preview = render_completions_text(default_shell, "lg");
        let log_tail = daemon::read_log_tail(&state_paths.log_file(), 200).unwrap_or_default();
        let alias_report = daemon::inspect_alias_status("lg", None).ok();
        let config_inspection = config_sync::inspect_config_sync(true, None).ok();

        let resolved_lang = prefs.lang.resolve_current();
        let sys_dark = is_appearance_dark(window.appearance());
        let resolved_dark = prefs.theme.resolve(sys_dark).is_dark();
        apply_staticflow_theme(resolved_dark, cx);

        let appearance_sub = cx.observe_window_appearance(window, |this, window, cx| {
            if this.theme_pref.is_system() {
                let now_dark = is_appearance_dark(window.appearance());
                if this.dark_mode != now_dark {
                    this.dark_mode = now_dark;
                    apply_staticflow_theme(now_dark, cx);
                    cx.notify();
                }
            }
        });

        let mut app = Self {
            rt,
            state_paths,
            current_exe,
            lang_pref: prefs.lang,
            theme_pref: prefs.theme,
            lang: resolved_lang,
            dark_mode: resolved_dark,
            _appearance_sub: appearance_sub,
            active_tab: NavTab::Overview,
            banner: None,
            busy_gateway: false,
            busy_launcher: false,
            busy_config: false,
            busy_models: false,
            busy_agy: false,
            busy_proxy: false,
            busy_doctor: false,
            busy_upgrade: false,
            gateway_report: None,
            live_telemetry: None,
            sse_connected: false,
            sse_target_key: None,
            sse_cancel_tx: None,
            gateway_form_hydrated: false,
            in_listen,
            in_ui_listen,
            in_default_model,
            in_acp_mode,
            in_api_key,
            in_max_idle_sessions,
            in_turn_timeout_secs,
            cfg_long_context: init_long_ctx,
            cfg_agy_auto_update: init_auto_upd,
            in_task_filter,
            protocol_filter: TaskProtocolFilter::All,
            selected_task_id: None,
            in_launch_dir,
            in_launch_model,
            in_claude_extra,
            in_codex_extra,
            claude_tool_mode: ToolBridgeMode::CodeMode,
            codex_tool_mode: ToolBridgeMode::CodeMode,
            launch_bypass_perms: true,
            launch_hold_open: false,
            desktop_apps,
            desktop_restart_app: false,
            codex_app_ephemeral: false,
            last_launcher_output: String::new(),
            config_target: ConfigTarget::All,
            config_global: true,
            config_offline: false,
            config_no_bypass: false,
            config_no_code_mode: false,
            in_config_fast_model,
            config_inspection,
            config_preview_output: String::new(),
            models_report: None,
            in_model_search,
            model_family: ModelFamilyFilter::All,
            agy_report: None,
            in_agy_custom_args,
            agy_terminal_output: String::new(),
            proxy_outcome: None,
            in_proxy_target,
            log_tail,
            log_lines_limit: 200,
            log_auto_follow: true,
            in_log_filter,
            doctor_report: None,
            alias_report,
            in_alias_name,
            selected_shell: default_shell,
            completions_preview,
            upgrade_outcome: None,
            in_upgrade_version,
            upgrade_force: false,
        };

        app.sync_localized_ui(window, cx);
        app.refresh_gateway_status(Some(window), cx);
        app.action_apply_proxy(ProxyApplyMode::RefreshOnly, cx);
        app.start_background_poller(cx);
        app
    }

    #[must_use]
    pub(crate) fn palette(&self) -> Palette {
        Palette::for_mode(self.dark_mode)
    }

    pub(crate) fn set_banner(
        &mut self,
        tone: BadgeTone,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) {
        self.banner = Some(StatusBanner {
            tone,
            title: title.into(),
            detail: detail.into(),
        });
    }

    pub(crate) fn copy_text(&mut self, label: &str, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        let title = tr(self.lang, "已复制到剪贴板", "Copied to clipboard");
        self.set_banner(BadgeTone::Ok, format!("{title}: {label}"), text);
        cx.notify();
    }

    pub(crate) fn persist_prefs(&self) {
        let mut prefs = self.state_paths.read_desktop_prefs().unwrap_or_default();
        prefs.lang = self.lang_pref;
        prefs.theme = self.theme_pref;
        let _ = self.state_paths.write_desktop_prefs(&prefs);
    }

    pub(crate) fn sync_localized_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let lang = self.lang;
        gpui_kit::component::set_locale(lang.gpui_locale());
        window.set_window_title(window_title(lang));

        let ui_listen_ph = tr(
            lang,
            "可选，如 127.0.0.1:8788",
            "optional, e.g. 127.0.0.1:8788",
        );
        self.in_ui_listen
            .update(cx, |s, cx| s.set_placeholder(ui_listen_ph, window, cx));
        let api_key_ph = tr(
            lang,
            "可选 Bearer / x-api-key（留空则免鉴权）",
            "optional Bearer / x-api-key",
        );
        self.in_api_key
            .update(cx, |s, cx| s.set_placeholder(api_key_ph, window, cx));
        let task_ph = tr(
            lang,
            "搜索模型、会话 ID、Prompt 或工具名…",
            "Search model, session, prompt or tool...",
        );
        self.in_task_filter
            .update(cx, |s, cx| s.set_placeholder(task_ph, window, cx));
        let dir_ph = tr(lang, "项目工作目录", "Project working directory");
        self.in_launch_dir
            .update(cx, |s, cx| s.set_placeholder(dir_ph, window, cx));
        let launch_mdl_ph = tr(
            lang,
            "可选模型覆盖（留空则使用网关默认模型）",
            "Optional model override (leave blank for gateway default)",
        );
        self.in_launch_model
            .update(cx, |s, cx| s.set_placeholder(launch_mdl_ph, window, cx));
        let claude_ph = tr(
            lang,
            "可选 Claude 参数，如 --continue 或 -p \"review diff\"",
            "Optional claude args, e.g. --continue or -p \"review diff\"",
        );
        self.in_claude_extra
            .update(cx, |s, cx| s.set_placeholder(claude_ph, window, cx));
        let codex_ph = tr(
            lang,
            "可选 Codex 参数，如 resume --last 或 exec \"cargo test\"",
            "Optional codex args, e.g. resume --last or exec \"cargo test\"",
        );
        self.in_codex_extra
            .update(cx, |s, cx| s.set_placeholder(codex_ph, window, cx));
        let fast_ph = tr(
            lang,
            "可选 Claude 子智能体快速模型覆盖",
            "Optional Claude fast/subagent model override",
        );
        self.in_config_fast_model
            .update(cx, |s, cx| s.set_placeholder(fast_ph, window, cx));
        let mdl_search_ph = tr(
            lang,
            "按模型 ID 或展示名称筛选…",
            "Filter by model ID or display name...",
        );
        self.in_model_search
            .update(cx, |s, cx| s.set_placeholder(mdl_search_ph, window, cx));
        let log_ph = tr(lang, "按关键词过滤日志行…", "Filter log lines...");
        self.in_log_filter
            .update(cx, |s, cx| s.set_placeholder(log_ph, window, cx));
        let upg_ph = tr(
            lang,
            "可选版本标签（如 v0.1.8）；留空自动拉取最新正式版",
            "Optional tag (e.g. v0.1.8); empty = latest release",
        );
        self.in_upgrade_version
            .update(cx, |s, cx| s.set_placeholder(upg_ph, window, cx));
    }

    pub(crate) fn toggle_language(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.lang = self.lang.toggled();
        self.lang_pref = match self.lang {
            DesktopLang::Zh => DesktopLangPref::Zh,
            DesktopLang::En => DesktopLangPref::En,
        };
        self.sync_localized_ui(window, cx);
        self.persist_prefs();
        cx.notify();
    }

    pub(crate) fn toggle_dark_mode(&mut self, cx: &mut Context<Self>) {
        self.dark_mode = !self.dark_mode;
        self.theme_pref = if self.dark_mode {
            DesktopThemePref::Dark
        } else {
            DesktopThemePref::Light
        };
        apply_staticflow_theme(self.dark_mode, cx);
        self.persist_prefs();
        cx.notify();
    }

    pub(crate) fn reset_prefs_to_system(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.lang_pref = DesktopLangPref::System;
        self.theme_pref = DesktopThemePref::System;
        self.lang = detect_system_language();
        self.dark_mode = is_appearance_dark(window.appearance());
        apply_staticflow_theme(self.dark_mode, cx);
        self.sync_localized_ui(window, cx);
        self.persist_prefs();
        cx.notify();
    }

    pub(crate) fn select_tab(&mut self, tab: NavTab, cx: &mut Context<Self>) {
        self.active_tab = tab;
        match tab {
            NavTab::Overview | NavTab::Telemetry => {
                self.refresh_gateway_status(None, cx);
            }
            NavTab::Launchers => {
                self.desktop_apps = desktop::inspect_desktop_apps(&self.state_paths);
                self.refresh_config_inspection(cx);
            }
            NavTab::Models => {
                if self.models_report.is_none() && !self.busy_models {
                    self.action_refresh_models(false, cx);
                }
            }
            NavTab::AgyProxy => {
                if self.agy_report.is_none() && !self.busy_agy {
                    self.action_refresh_agy_status(false, cx);
                }
                if self.proxy_outcome.is_none() && !self.busy_proxy {
                    self.action_apply_proxy(ProxyApplyMode::RefreshOnly, cx);
                }
            }
            NavTab::System => {
                self.refresh_logs(cx);
                self.refresh_alias_and_completions(cx);
                if self.doctor_report.is_none() && !self.busy_doctor {
                    self.action_run_doctor(cx);
                }
            }
        }
        cx.notify();
    }

    fn start_background_poller(&mut self, cx: &mut Context<Self>) {
        let rt = self.rt.clone();
        let (tick_tx, mut tick_rx) = watch::channel::<u64>(0);
        rt.spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(3));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            interval.tick().await;
            let mut seq = 0u64;
            loop {
                interval.tick().await;
                seq = seq.wrapping_add(1);
                if tick_tx.send(seq).is_err() {
                    break;
                }
            }
        });

        cx.spawn(async move |this, cx| {
            while tick_rx.changed().await.is_ok() {
                let _ = *tick_rx.borrow_and_update();
                let alive = this.update(cx, |app, cx| {
                    if !app.busy_gateway {
                        app.refresh_gateway_status(None, cx);
                    }
                    if app.log_auto_follow
                        && matches!(app.active_tab, NavTab::System | NavTab::Overview)
                    {
                        app.refresh_logs(cx);
                    }
                    if app.active_tab == NavTab::Launchers && !app.busy_launcher {
                        app.desktop_apps = desktop::inspect_desktop_apps(&app.state_paths);
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    // =========================================================================
    // Gateway & SSE Live Telemetry
    // =========================================================================

    pub(crate) fn refresh_gateway_status(
        &mut self,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        let state_paths = self.state_paths.clone();
        let explicit_listen = self.parse_listen_override(cx);
        let explicit_key = self.read_optional_input(&self.in_api_key, cx);
        let rt = self.rt.clone();
        let (tx, rx) = oneshot::channel();

        rt.spawn(async move {
            let res = daemon::inspect_gateway_status(
                &state_paths,
                &StatusArgs {
                    listen: explicit_listen,
                    api_key: explicit_key,
                    json: false,
                },
            )
            .await;
            let _ = tx.send(res);
        });

        if let Some(win) = window {
            cx.spawn_in(win, async move |this, cx| {
                if let Ok(Ok(report)) = rx.await {
                    let _ = this.update_in(cx, |app, window, cx| {
                        app.apply_gateway_report(report, Some(window), cx);
                    });
                }
            })
            .detach();
        } else {
            cx.spawn(async move |this, cx| {
                if let Ok(Ok(report)) = rx.await {
                    let _ = this.update(cx, |app, cx| {
                        app.apply_gateway_report(report, None, cx);
                    });
                }
            })
            .detach();
        }
    }

    fn apply_gateway_report(
        &mut self,
        report: GatewayStatusReport,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        let resolved_key = daemon::resolve_client_api_key(
            self.read_optional_input(&self.in_api_key, cx).as_deref(),
            report.managed_daemon.as_ref(),
            report.listen,
        );

        if !self.gateway_form_hydrated
            && let Some(win) = window
        {
            self.hydrate_form_from_status(&report, win, cx);
            self.gateway_form_hydrated = true;
        }

        if let Some(snap) = &report.telemetry {
            self.live_telemetry = Some(snap.clone());
        }
        if let Some(models) = &report.models
            && self.models_report.is_none()
        {
            let def_model = report
                .managed_daemon
                .as_ref()
                .map(|d| d.default_model.clone())
                .or_else(|| {
                    report
                        .telemetry
                        .as_ref()
                        .map(|t| t.acp_process.default_model.clone())
                })
                .unwrap_or_else(|| "gemini-4-carbon-high".to_owned());
            let fast = config_sync::resolve_fast_model(None, &def_model, models);
            let title_mdl = std::fs::read_to_string(self.state_paths.title_model_file())
                .ok()
                .and_then(|s| liftgate_types::config::ModelId::try_new(s).ok())
                .unwrap_or_else(liftgate_types::config::ModelId::fallback_fast)
                .as_str()
                .to_owned();
            self.models_report = Some(ModelsReport {
                gateway_url: report.gateway_url.clone(),
                default_model: def_model,
                fast_model: fast,
                title_model: title_mdl,
                long_context: report.long_context,
                models: models.clone(),
            });
        }

        if report.healthy {
            self.ensure_sse_stream(report.listen, resolved_key, cx);
        } else {
            self.stop_sse_stream();
        }

        self.gateway_report = Some(report);
        cx.notify();
    }

    pub(crate) fn hydrate_form_from_status(
        &mut self,
        report: &GatewayStatusReport,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let listen_str = report.listen.to_string();
        self.in_listen.update(cx, |s, cx| {
            s.set_value(listen_str, window, cx);
        });
        if let Some(d) = &report.managed_daemon {
            let ui_str = d.ui_listen.map_or_else(String::new, |u| u.to_string());
            let model_str = d.default_model.clone();
            let mode_str = d.acp_mode.clone();
            let key_str = d.api_key.clone().unwrap_or_default();
            let idle_str = d.max_idle_sessions.to_string();
            let timeout_str = d.turn_timeout_secs.to_string();
            self.in_ui_listen
                .update(cx, |s, cx| s.set_value(ui_str, window, cx));
            if !model_str.is_empty() {
                self.in_default_model
                    .update(cx, |s, cx| s.set_value(model_str, window, cx));
            }
            if !mode_str.is_empty() {
                self.in_acp_mode
                    .update(cx, |s, cx| s.set_value(mode_str, window, cx));
            }
            self.in_api_key
                .update(cx, |s, cx| s.set_value(key_str, window, cx));
            self.in_max_idle_sessions
                .update(cx, |s, cx| s.set_value(idle_str, window, cx));
            self.in_turn_timeout_secs
                .update(cx, |s, cx| s.set_value(timeout_str, window, cx));
            self.cfg_long_context = d.long_context;
            self.cfg_agy_auto_update = d.agy_auto_update;
        } else {
            self.cfg_long_context = report.long_context;
            self.cfg_agy_auto_update = report.agy_auto_update_enabled;
        }
    }

    fn ensure_sse_stream(
        &mut self,
        listen: SocketAddr,
        api_key: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let target = (listen, api_key.clone());
        if self.sse_connected && self.sse_target_key.as_ref() == Some(&target) {
            return;
        }
        self.stop_sse_stream();
        self.sse_target_key = Some(target);
        self.sse_connected = true;

        let (cancel_tx, cancel_rx) = watch::channel(false);
        self.sse_cancel_tx = Some(cancel_tx);
        let (snap_tx, mut snap_rx) = watch::channel::<Option<GatewayTelemetrySnapshot>>(None);
        let client = GatewayClient::new(listen, api_key);
        let rt = self.rt.clone();

        rt.spawn(async move {
            let _ = client.subscribe_telemetry_events(snap_tx, cancel_rx).await;
        });

        cx.spawn(async move |this, cx| {
            while snap_rx.changed().await.is_ok() {
                let Some(snap) = snap_rx.borrow_and_update().clone() else {
                    continue;
                };
                let ok = this.update(cx, |app, cx| {
                    app.sse_connected = true;
                    if app.selected_task_id.is_none() {
                        app.selected_task_id = snap
                            .active_tasks
                            .first()
                            .or_else(|| snap.recent_tasks.first())
                            .map(|t| t.task_id.clone());
                    }
                    app.live_telemetry = Some(snap);
                    cx.notify();
                });
                if ok.is_err() {
                    break;
                }
            }
            let _ = this.update(cx, |app, cx| {
                app.sse_connected = false;
                cx.notify();
            });
        })
        .detach();
    }

    fn stop_sse_stream(&mut self) {
        if let Some(tx) = self.sse_cancel_tx.take() {
            let _ = tx.send(true);
        }
        self.sse_connected = false;
        self.sse_target_key = None;
    }

    fn read_optional_input(&self, input: &Entity<InputState>, cx: &App) -> Option<String> {
        let raw = input.read(cx).value().trim().to_owned();
        if raw.is_empty() { None } else { Some(raw) }
    }

    fn parse_listen_override(&self, cx: &App) -> Option<SocketAddr> {
        self.read_optional_input(&self.in_listen, cx)
            .and_then(|s| s.parse::<SocketAddr>().ok())
    }

    pub(crate) fn build_serve_args_from_form(&self, cx: &App) -> Result<ServeArgs, String> {
        let defaults = ServeArgs::default();
        let mut serve = self
            .state_paths
            .read_daemon()
            .ok()
            .flatten()
            .as_ref()
            .map_or_else(ServeArgs::default, daemon::serve_args_from_daemon_state);
        let listen_raw = self.in_listen.read(cx).value().trim().to_owned();
        if listen_raw.is_empty() {
            serve.listen = defaults.listen;
        } else {
            serve.listen = listen_raw.parse::<SocketAddr>().map_err(|e| {
                format!(
                    "{}: `{listen_raw}` ({e})",
                    tr(self.lang, "监听地址格式无效", "Invalid listen address")
                )
            })?;
        }
        serve.ui_listen = match self.read_optional_input(&self.in_ui_listen, cx) {
            Some(ui_raw) => Some(ui_raw.parse::<SocketAddr>().map_err(|e| {
                format!(
                    "{}: `{ui_raw}` ({e})",
                    tr(self.lang, "面板监听地址无效", "Invalid UI listen address")
                )
            })?),
            None => None,
        };
        serve.default_model = self
            .read_optional_input(&self.in_default_model, cx)
            .unwrap_or(defaults.default_model);
        serve.acp_mode = self
            .read_optional_input(&self.in_acp_mode, cx)
            .unwrap_or(defaults.acp_mode);
        serve.api_key =
            normalize_optional_api_key(self.read_optional_input(&self.in_api_key, cx).as_deref());
        if let Some(idle_raw) = self.read_optional_input(&self.in_max_idle_sessions, cx) {
            serve.max_idle_sessions = idle_raw.parse::<usize>().map_err(|e| {
                format!(
                    "{}: `{idle_raw}` ({e})",
                    tr(self.lang, "最大空闲会话数无效", "Invalid max idle sessions")
                )
            })?;
        } else {
            serve.max_idle_sessions = defaults.max_idle_sessions;
        }
        if let Some(timeout_raw) = self.read_optional_input(&self.in_turn_timeout_secs, cx) {
            serve.turn_timeout_secs = timeout_raw.parse::<u64>().map_err(|e| {
                format!(
                    "{}: `{timeout_raw}` ({e})",
                    tr(
                        self.lang,
                        "单轮超时秒数无效",
                        "Invalid turn timeout seconds"
                    )
                )
            })?;
        } else {
            serve.turn_timeout_secs = defaults.turn_timeout_secs;
        }
        serve.long_context = Some(self.cfg_long_context);
        serve.agy_auto_update = Some(self.cfg_agy_auto_update);
        serve.no_agy_auto_update = !self.cfg_agy_auto_update;
        Ok(serve)
    }

    pub(crate) fn action_start_gateway(
        &mut self,
        force_restart: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy_gateway {
            return;
        }
        let serve = match self.build_serve_args_from_form(cx) {
            Ok(s) => s,
            Err(msg) => {
                self.set_banner(
                    BadgeTone::Err,
                    tr(self.lang, "网关参数校验失败", "Invalid Gateway Parameters"),
                    msg,
                );
                cx.notify();
                return;
            }
        };

        self.busy_gateway = true;
        cx.notify();

        let state_paths = self.state_paths.clone();
        let exe = self.current_exe.clone();
        let rt = self.rt.clone();
        let (tx, rx) = oneshot::channel();

        rt.spawn(async move {
            if let Ok(mut settings) = state_paths.read_settings() {
                if let Some(lc) = serve.long_context {
                    settings.long_context = lc;
                }
                if let Some(au) = serve.agy_auto_update {
                    settings.agy_auto_update = au;
                }
                let _ = state_paths.write_settings(&settings);
            }
            let start_res =
                daemon::start_daemon_with_exe(&state_paths, Some(&exe), serve, force_restart).await;
            let status_res =
                daemon::inspect_gateway_status(&state_paths, &StatusArgs::default()).await;
            let _ = tx.send((start_res, status_res));
        });

        cx.spawn_in(window, async move |this, cx| {
            if let Ok((start_res, status_res)) = rx.await {
                let _ = this.update_in(cx, |app, window, cx| {
                    app.busy_gateway = false;
                    match start_res {
                        Ok(outcome) => {
                            let headline = if force_restart {
                                tr(app.lang, "后台网关已重启", "Gateway Restarted")
                            } else if outcome.already_running {
                                tr(app.lang, "后台网关已在运行", "Gateway Already Online")
                            } else {
                                tr(app.lang, "后台网关已启动", "Gateway Started")
                            };
                            app.set_banner(
                                BadgeTone::Ok,
                                headline,
                                if app.lang.is_en() {
                                    format!(
                                        "PID {} · {} · {} models",
                                        outcome.daemon.pid,
                                        outcome.daemon.connect_base_url(),
                                        outcome.live_model_count
                                    )
                                } else {
                                    format!(
                                        "PID {} · {} · {} 个模型",
                                        outcome.daemon.pid,
                                        outcome.daemon.connect_base_url(),
                                        outcome.live_model_count
                                    )
                                },
                            );
                        }
                        Err(err) => {
                            app.set_banner(
                                BadgeTone::Err,
                                tr(app.lang, "启动网关失败", "Failed to Start Gateway"),
                                err.to_string(),
                            );
                        }
                    }
                    if let Ok(rep) = status_res {
                        app.apply_gateway_report(rep, Some(window), cx);
                    }
                    app.refresh_logs(cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(crate) fn action_stop_gateway(&mut self, cx: &mut Context<Self>) {
        if self.busy_gateway {
            return;
        }
        self.busy_gateway = true;
        self.stop_sse_stream();
        cx.notify();

        let state_paths = self.state_paths.clone();
        let rt = self.rt.clone();
        let (tx, rx) = oneshot::channel();

        rt.spawn(async move {
            let stop_res = daemon::stop_daemon(&state_paths).await;
            let status_res =
                daemon::inspect_gateway_status(&state_paths, &StatusArgs::default()).await;
            let _ = tx.send((stop_res, status_res));
        });

        cx.spawn(async move |this, cx| {
            if let Ok((stop_res, status_res)) = rx.await {
                let _ = this.update(cx, |app, cx| {
                    app.busy_gateway = false;
                    match stop_res {
                        Ok(outcome) => {
                            let detail = match outcome.stopped_pid {
                                Some(pid) => {
                                    if app.lang.is_en() {
                                        format!("Stopped PID {pid}")
                                    } else {
                                        format!("已终止守护进程 PID {pid}")
                                    }
                                }
                                None => tr(
                                    app.lang,
                                    "当前没有托管中的网关进程",
                                    "No managed daemon was running",
                                )
                                .to_owned(),
                            };
                            app.set_banner(
                                BadgeTone::Info,
                                tr(app.lang, "网关已停止", "Gateway Stopped"),
                                detail,
                            );
                        }
                        Err(err) => {
                            app.set_banner(
                                BadgeTone::Err,
                                tr(app.lang, "停止网关失败", "Failed to Stop Gateway"),
                                err.to_string(),
                            );
                        }
                    }
                    if let Ok(rep) = status_res {
                        app.apply_gateway_report(rep, None, cx);
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(crate) fn action_open_web_dashboard(&mut self, cx: &mut Context<Self>) {
        let state_paths = self.state_paths.clone();
        let listen = self.parse_listen_override(cx);
        let api_key = self.read_optional_input(&self.in_api_key, cx);
        let rt = self.rt.clone();
        let (tx, rx) = oneshot::channel();

        rt.spawn(async move {
            let res = daemon::resolve_ui_target(
                &state_paths,
                &UiArgs {
                    listen,
                    api_key,
                    print: false,
                    no_start: false,
                },
            )
            .await
            .and_then(|target| {
                open_browser(&target.authenticated_url)?;
                Ok(target.display_url)
            });
            let _ = tx.send(res);
        });

        cx.spawn(async move |this, cx| {
            if let Ok(res) = rx.await {
                let _ = this.update(cx, |app, cx| {
                    match res {
                        Ok(url) => app.set_banner(
                            BadgeTone::Ok,
                            tr(app.lang, "已在浏览器打开监控面板", "Opened Web Dashboard"),
                            url,
                        ),
                        Err(err) => app.set_banner(
                            BadgeTone::Err,
                            tr(app.lang, "打开面板失败", "Failed to Open Dashboard"),
                            err.to_string(),
                        ),
                    }
                    app.refresh_gateway_status(None, cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(crate) fn action_set_default_model(
        &mut self,
        model_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let clean = model_id.trim().to_owned();
        if clean.is_empty() || self.busy_gateway {
            return;
        }
        self.in_default_model
            .update(cx, |s, cx| s.set_value(clean.clone(), window, cx));
        self.busy_gateway = true;
        cx.notify();

        let state_paths = self.state_paths.clone();
        let rt = self.rt.clone();
        let model_for_restart = clean.clone();
        let (tx, rx) = oneshot::channel();

        rt.spawn(async move {
            let restart_res = daemon::restart_daemon(
                &state_paths,
                RestartArgs {
                    default_model: Some(model_for_restart),
                    ..RestartArgs::default()
                },
            )
            .await;
            let status_res =
                daemon::inspect_gateway_status(&state_paths, &StatusArgs::default()).await;
            let models_res = daemon::inspect_models(
                &state_paths,
                &ModelsArgs {
                    listen: None,
                    api_key: None,
                    pick: false,
                    json: false,
                    no_start: true,
                },
            )
            .await;
            let _ = tx.send((restart_res, status_res, models_res));
        });

        cx.spawn_in(window, async move |this, cx| {
            if let Ok((restart_res, status_res, models_res)) = rx.await {
                let _ = this.update_in(cx, |app, window, cx| {
                    app.busy_gateway = false;
                    match restart_res {
                        Ok(outcome) => app.set_banner(
                            BadgeTone::Ok,
                            tr(app.lang, "默认模型已更新", "Default Model Updated"),
                            format!(
                                "{} (PID {})",
                                outcome.daemon.default_model, outcome.daemon.pid
                            ),
                        ),
                        Err(err) => app.set_banner(
                            BadgeTone::Err,
                            tr(
                                app.lang,
                                "切换默认模型失败",
                                "Failed to Switch Default Model",
                            ),
                            err.to_string(),
                        ),
                    }
                    if let Ok(rep) = status_res {
                        app.apply_gateway_report(rep, Some(window), cx);
                    }
                    if let Ok(m) = models_res {
                        app.models_report = Some(m);
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(crate) fn action_set_title_model(&mut self, model_id: String, cx: &mut Context<Self>) {
        let clean = model_id.trim().to_owned();
        if clean.is_empty() {
            return;
        }
        match daemon::set_title_model(&self.state_paths, &clean) {
            Ok(validated) => {
                let next_id = validated.as_str().to_owned();
                if let Some(rep) = &mut self.models_report {
                    rep.title_model.clone_from(&next_id);
                }
                self.set_banner(
                    BadgeTone::Ok,
                    tr(
                        self.lang,
                        "会话标题生成模型已更新（实时生效）",
                        "Session Title Model Updated",
                    ),
                    next_id,
                );
            }
            Err(err) => {
                self.set_banner(
                    BadgeTone::Err,
                    tr(
                        self.lang,
                        "设置会话标题模型失败",
                        "Failed to Set Title Model",
                    ),
                    err.to_string(),
                );
            }
        }
        cx.notify();
    }

    pub(crate) fn action_evict_session(&mut self, session_id: String, cx: &mut Context<Self>) {
        let Some(report) = &self.gateway_report else {
            return;
        };
        let listen = report.listen;
        let key = daemon::resolve_client_api_key(
            self.read_optional_input(&self.in_api_key, cx).as_deref(),
            report.managed_daemon.as_ref(),
            listen,
        );
        let rt = self.rt.clone();
        let sid = session_id.clone();
        let (tx, rx) = oneshot::channel();

        rt.spawn(async move {
            let client = GatewayClient::new(listen, key);
            let res = client.close_idle_session(&sid).await;
            let _ = tx.send(res);
        });

        cx.spawn(async move |this, cx| {
            if let Ok(res) = rx.await {
                let _ = this.update(cx, |app, cx| {
                    match res {
                        Ok(true) => app.set_banner(
                            BadgeTone::Ok,
                            tr(app.lang, "已驱逐空闲热会话", "Evicted Idle Warm Session"),
                            session_id,
                        ),
                        Ok(false) => app.set_banner(
                            BadgeTone::Warn,
                            tr(
                                app.lang,
                                "会话正忙或已关闭",
                                "Session Busy or Already Closed",
                            ),
                            session_id,
                        ),
                        Err(err) => app.set_banner(
                            BadgeTone::Err,
                            tr(app.lang, "关闭会话失败", "Failed to Evict Session"),
                            err.to_string(),
                        ),
                    }
                    app.refresh_gateway_status(None, cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    // =========================================================================
    // AI Launchers & Persistent Config Sync
    // =========================================================================

    pub(crate) fn resolve_launch_cwd(&self, cx: &App) -> Option<PathBuf> {
        self.read_optional_input(&self.in_launch_dir, cx)
            .map(PathBuf::from)
    }

    pub(crate) fn build_claude_args(&self, model_override: Option<String>, cx: &App) -> ClaudeArgs {
        let model = model_override.or_else(|| self.read_optional_input(&self.in_launch_model, cx));
        let extra = self
            .read_optional_input(&self.in_claude_extra, cx)
            .map(|s| parse_cli_tokens(&s))
            .unwrap_or_default();
        ClaudeArgs {
            listen: self.parse_listen_override(cx),
            api_key: self.read_optional_input(&self.in_api_key, cx),
            model,
            fast_model: self.read_optional_input(&self.in_config_fast_model, cx),
            no_bypass: !self.launch_bypass_perms,
            no_code_mode: self.claude_tool_mode == ToolBridgeMode::DirectTools,
            desktop: false,
            restart_app: false,
            wait: false,
            claude_desktop_bin: None,
            claude_bin: None,
            passthrough_args: extra,
        }
    }

    pub(crate) fn build_codex_args(&self, model_override: Option<String>, cx: &App) -> CodexArgs {
        let model = model_override.or_else(|| self.read_optional_input(&self.in_launch_model, cx));
        let extra = self
            .read_optional_input(&self.in_codex_extra, cx)
            .map(|s| parse_cli_tokens(&s))
            .unwrap_or_default();
        CodexArgs {
            listen: self.parse_listen_override(cx),
            api_key: self.read_optional_input(&self.in_api_key, cx),
            model,
            no_bypass: !self.launch_bypass_perms,
            code_mode: self.codex_tool_mode == ToolBridgeMode::CodeMode,
            no_code_mode: self.codex_tool_mode == ToolBridgeMode::DirectTools,
            code_mode_only: self.codex_tool_mode == ToolBridgeMode::CodeModeOnly,
            app: false,
            restart_app: false,
            ephemeral: false,
            wait: false,
            codex_bin: None,
            codex_app_bin: None,
            passthrough_args: extra,
        }
    }

    pub(crate) fn action_launch_claude_cli(
        &mut self,
        model_override: Option<String>,
        preview_only: bool,
        cx: &mut Context<Self>,
    ) {
        if self.busy_launcher {
            return;
        }
        let args = self.build_claude_args(model_override, cx);
        let cwd = self.resolve_launch_cwd(cx);
        let hold_open = self.launch_hold_open;
        let state_paths = self.state_paths.clone();
        let rt = self.rt.clone();
        self.busy_launcher = true;
        cx.notify();

        let (tx, rx) = oneshot::channel();
        rt.spawn(async move {
            let res = harness::prepare_claude_terminal_spec(&state_paths, &args, cwd, hold_open)
                .await
                .and_then(|spec| {
                    if preview_only {
                        prepare_reusable_launcher_command(&state_paths, "claude", &spec)
                    } else {
                        let preview = spec.preview_command_line();
                        spawn_in_system_terminal(&state_paths, &spec)?;
                        Ok(preview)
                    }
                });
            let _ = tx.send(res);
        });

        cx.spawn(async move |this, cx| {
            if let Ok(res) = rx.await {
                let _ = this.update(cx, |app, cx| {
                    app.busy_launcher = false;
                    match res {
                        Ok(cmd_line) => {
                            app.last_launcher_output = format!("$ {cmd_line}");
                            if preview_only {
                                app.copy_text(
                                    tr(app.lang, "Claude 启动命令", "Claude command"),
                                    cmd_line,
                                    cx,
                                );
                            } else {
                                app.set_banner(
                                    BadgeTone::Ok,
                                    tr(
                                        app.lang,
                                        "已在系统终端启动 Claude Code",
                                        "Launched Claude Code in Terminal",
                                    ),
                                    cmd_line,
                                );
                            }
                        }
                        Err(err) => {
                            app.last_launcher_output = format!("ERROR: {err}");
                            app.set_banner(
                                BadgeTone::Err,
                                tr(
                                    app.lang,
                                    "启动 Claude Code 失败",
                                    "Failed to Launch Claude Code",
                                ),
                                err.to_string(),
                            );
                        }
                    }
                    app.refresh_gateway_status(None, cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(crate) fn action_launch_codex_cli(
        &mut self,
        model_override: Option<String>,
        preview_only: bool,
        cx: &mut Context<Self>,
    ) {
        if self.busy_launcher {
            return;
        }
        let args = self.build_codex_args(model_override, cx);
        let cwd = self.resolve_launch_cwd(cx);
        let hold_open = self.launch_hold_open;
        let state_paths = self.state_paths.clone();
        let rt = self.rt.clone();
        self.busy_launcher = true;
        cx.notify();

        let (tx, rx) = oneshot::channel();
        rt.spawn(async move {
            let res = harness::prepare_codex_terminal_spec(&state_paths, &args, cwd, hold_open)
                .await
                .and_then(|spec| {
                    if preview_only {
                        prepare_reusable_launcher_command(&state_paths, "codex", &spec)
                    } else {
                        let preview = spec.preview_command_line();
                        spawn_in_system_terminal(&state_paths, &spec)?;
                        Ok(preview)
                    }
                });
            let _ = tx.send(res);
        });

        cx.spawn(async move |this, cx| {
            if let Ok(res) = rx.await {
                let _ = this.update(cx, |app, cx| {
                    app.busy_launcher = false;
                    match res {
                        Ok(cmd_line) => {
                            app.last_launcher_output = format!("$ {cmd_line}");
                            if preview_only {
                                app.copy_text(
                                    tr(app.lang, "Codex 启动命令", "Codex command"),
                                    cmd_line,
                                    cx,
                                );
                            } else {
                                app.set_banner(
                                    BadgeTone::Ok,
                                    tr(
                                        app.lang,
                                        "已在系统终端启动 Codex CLI",
                                        "Launched Codex CLI in Terminal",
                                    ),
                                    cmd_line,
                                );
                            }
                        }
                        Err(err) => {
                            app.last_launcher_output = format!("ERROR: {err}");
                            app.set_banner(
                                BadgeTone::Err,
                                tr(
                                    app.lang,
                                    "启动 Codex CLI 失败",
                                    "Failed to Launch Codex CLI",
                                ),
                                err.to_string(),
                            );
                        }
                    }
                    app.refresh_gateway_status(None, cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(crate) fn action_launch_codex_app(
        &mut self,
        model_override: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.busy_launcher {
            return;
        }
        let args = CodexAppArgs {
            listen: self.parse_listen_override(cx),
            api_key: self.read_optional_input(&self.in_api_key, cx),
            model: model_override.or_else(|| self.read_optional_input(&self.in_launch_model, cx)),
            no_bypass: !self.launch_bypass_perms,
            code_mode: self.codex_tool_mode == ToolBridgeMode::CodeMode,
            code_mode_only: self.codex_tool_mode == ToolBridgeMode::CodeModeOnly,
            no_code_mode: self.codex_tool_mode == ToolBridgeMode::DirectTools,
            restart_app: self.desktop_restart_app,
            ephemeral: self.codex_app_ephemeral,
            wait: false,
            codex_app_bin: None,
            codex_bin: None,
            passthrough_args: Vec::new(),
        };
        let state_paths = self.state_paths.clone();
        let rt = self.rt.clone();
        self.busy_launcher = true;
        cx.notify();

        let (tx, rx) = oneshot::channel();
        rt.spawn(async move {
            let res = desktop::launch_codex_app(&state_paths, args).await;
            let _ = tx.send(res);
        });

        cx.spawn(async move |this, cx| {
            if let Ok(res) = rx.await {
                let _ = this.update(cx, |app, cx| {
                    app.busy_launcher = false;
                    app.desktop_apps = desktop::inspect_desktop_apps(&app.state_paths);
                    match res {
                        Ok(outcome) => {
                            let mut buf = format!("# {}\n", outcome.headline);
                            for (k, v) in &outcome.rows {
                                let _ = writeln!(buf, "{k:<18} {v}");
                            }
                            app.last_launcher_output = buf;
                            app.set_banner(
                                BadgeTone::Ok,
                                tr(
                                    app.lang,
                                    "已拉起隔离会话版 Codex App",
                                    "Launched Isolated Codex App",
                                ),
                                outcome.headline,
                            );
                        }
                        Err(err) => {
                            app.last_launcher_output = format!("ERROR: {err}");
                            app.set_banner(
                                BadgeTone::Err,
                                tr(
                                    app.lang,
                                    "启动 Codex App 失败",
                                    "Failed to Launch Codex App",
                                ),
                                err.to_string(),
                            );
                        }
                    }
                    app.refresh_gateway_status(None, cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(crate) fn action_launch_claude_desktop(
        &mut self,
        restore_only: bool,
        model_override: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.busy_launcher {
            return;
        }
        let args = ClaudeDesktopArgs {
            listen: self.parse_listen_override(cx),
            api_key: self.read_optional_input(&self.in_api_key, cx),
            model: model_override.or_else(|| self.read_optional_input(&self.in_launch_model, cx)),
            restart_app: self.desktop_restart_app,
            restore: restore_only,
            wait: false,
            claude_desktop_bin: None,
            passthrough_args: Vec::new(),
        };
        let state_paths = self.state_paths.clone();
        let rt = self.rt.clone();
        self.busy_launcher = true;
        cx.notify();

        let (tx, rx) = oneshot::channel();
        rt.spawn(async move {
            let res = desktop::launch_claude_desktop(&state_paths, args).await;
            let _ = tx.send(res);
        });

        cx.spawn(async move |this, cx| {
            if let Ok(res) = rx.await {
                let _ = this.update(cx, |app, cx| {
                    app.busy_launcher = false;
                    app.desktop_apps = desktop::inspect_desktop_apps(&app.state_paths);
                    match res {
                        Ok(outcome) => {
                            let mut buf = format!("# {}\n", outcome.headline);
                            for (k, v) in &outcome.rows {
                                let _ = writeln!(buf, "{k:<18} {v}");
                            }
                            app.last_launcher_output = buf;
                            let title = if restore_only {
                                tr(
                                    app.lang,
                                    "Claude Desktop 会话配置已恢复",
                                    "Restored Claude Desktop State",
                                )
                            } else {
                                tr(
                                    app.lang,
                                    "已拉起 3P 模式 Claude Desktop",
                                    "Launched Claude Desktop (3P)",
                                )
                            };
                            app.set_banner(BadgeTone::Ok, title, outcome.headline);
                        }
                        Err(err) => {
                            app.last_launcher_output = format!("ERROR: {err}");
                            app.set_banner(
                                BadgeTone::Err,
                                tr(
                                    app.lang,
                                    "Claude Desktop 操作失败",
                                    "Claude Desktop Action Failed",
                                ),
                                err.to_string(),
                            );
                        }
                    }
                    app.refresh_gateway_status(None, cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(crate) fn refresh_config_inspection(&mut self, cx: &mut Context<Self>) {
        let dir = if self.config_global {
            None
        } else {
            self.resolve_launch_cwd(cx)
        };
        self.config_inspection =
            config_sync::inspect_config_sync(self.config_global, dir.as_deref()).ok();
    }

    pub(crate) fn action_run_config_sync(
        &mut self,
        dry_run_print: bool,
        uninstall: bool,
        cx: &mut Context<Self>,
    ) {
        if self.busy_config {
            return;
        }
        let args = ConfigArgs {
            target: self.config_target,
            global: self.config_global,
            dir: if self.config_global {
                None
            } else {
                self.resolve_launch_cwd(cx)
            },
            listen: self.parse_listen_override(cx),
            api_key: self.read_optional_input(&self.in_api_key, cx),
            model: self.read_optional_input(&self.in_launch_model, cx),
            fast_model: self.read_optional_input(&self.in_config_fast_model, cx),
            no_bypass: self.config_no_bypass,
            no_code_mode: self.config_no_code_mode,
            offline: self.config_offline,
            print: dry_run_print,
            uninstall,
        };

        self.busy_config = true;
        cx.notify();

        let state_paths = self.state_paths.clone();
        let rt = self.rt.clone();
        let (tx, rx) = oneshot::channel();

        rt.spawn(async move {
            let res = config_sync::execute_config_sync(&state_paths, &args).await;
            let _ = tx.send(res);
        });

        cx.spawn(async move |this, cx| {
            if let Ok(res) = rx.await {
                let _ = this.update(cx, |app, cx| {
                    app.busy_config = false;
                    app.refresh_config_inspection(cx);
                    match res {
                        Ok(outcome) => {
                            let mut preview = String::new();
                            for item in &outcome.results {
                                let _ = writeln!(
                                    preview,
                                    "# [{}] {} -> {}",
                                    item.target,
                                    item.action,
                                    item.config_path.display()
                                );
                                if let Some(cat) = &item.catalog_path {
                                    let _ = writeln!(preview, "# catalog: {}", cat.display());
                                }
                                if let Some(body) = &item.rendered_preview {
                                    let _ = writeln!(preview, "{body}");
                                }
                            }
                            app.config_preview_output = preview;
                            let title = if dry_run_print {
                                tr(
                                    app.lang,
                                    "已生成配置预览（未写盘）",
                                    "Generated Dry-Run Preview",
                                )
                            } else if uninstall {
                                tr(
                                    app.lang,
                                    "已移除 Liftgate 托管配置",
                                    "Removed Liftgate Config",
                                )
                            } else {
                                tr(app.lang, "持久化配置已同步", "Configuration Synced")
                            };
                            app.set_banner(
                                BadgeTone::Ok,
                                title,
                                if app.lang.is_en() {
                                    format!("{} target(s) processed", outcome.results.len())
                                } else {
                                    format!("已处理 {} 项配置目标", outcome.results.len())
                                },
                            );
                        }
                        Err(err) => {
                            app.config_preview_output = format!("ERROR: {err}");
                            app.set_banner(
                                BadgeTone::Err,
                                tr(app.lang, "配置同步失败", "Config Sync Failed"),
                                err.to_string(),
                            );
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    // =========================================================================
    // Models Catalog
    // =========================================================================

    pub(crate) fn action_refresh_models(&mut self, allow_auto_start: bool, cx: &mut Context<Self>) {
        if self.busy_models {
            return;
        }
        self.busy_models = true;
        cx.notify();

        let state_paths = self.state_paths.clone();
        let listen = self.parse_listen_override(cx);
        let api_key = self.read_optional_input(&self.in_api_key, cx);
        let rt = self.rt.clone();
        let (tx, rx) = oneshot::channel();

        rt.spawn(async move {
            let res = daemon::inspect_models(
                &state_paths,
                &ModelsArgs {
                    listen,
                    api_key,
                    pick: false,
                    json: false,
                    no_start: !allow_auto_start,
                },
            )
            .await;
            let _ = tx.send(res);
        });

        cx.spawn(async move |this, cx| {
            if let Ok(res) = rx.await {
                let _ = this.update(cx, |app, cx| {
                    app.busy_models = false;
                    match res {
                        Ok(report) => {
                            let count = report.models.len();
                            app.models_report = Some(report);
                            if allow_auto_start {
                                app.set_banner(
                                    BadgeTone::Ok,
                                    tr(app.lang, "模型目录已刷新", "Models Catalog Refreshed"),
                                    if app.lang.is_en() {
                                        format!("{count} models available")
                                    } else {
                                        format!("共发现 {count} 个可用模型")
                                    },
                                );
                                app.refresh_gateway_status(None, cx);
                            }
                        }
                        Err(err) => {
                            if allow_auto_start {
                                app.set_banner(
                                    BadgeTone::Err,
                                    tr(app.lang, "获取模型列表失败", "Failed to Fetch Models"),
                                    err.to_string(),
                                );
                            }
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    // =========================================================================
    // Antigravity (`agy`) & Network Proxy
    // =========================================================================

    pub(crate) fn action_refresh_agy_status(&mut self, check_auth: bool, cx: &mut Context<Self>) {
        if self.busy_agy {
            return;
        }
        self.busy_agy = true;
        cx.notify();

        let state_paths = self.state_paths.clone();
        let rt = self.rt.clone();
        let (tx, rx) = oneshot::channel();

        rt.spawn(async move {
            let res = agy_manager::inspect_agy_status(
                &state_paths,
                None,
                &AgyStatusArgs {
                    offline: false,
                    no_auth_check: !check_auth,
                },
            )
            .await;
            let _ = tx.send(res);
        });

        cx.spawn(async move |this, cx| {
            if let Ok(res) = rx.await {
                let _ = this.update(cx, |app, cx| {
                    app.busy_agy = false;
                    match res {
                        Ok(rep) => {
                            app.cfg_agy_auto_update = rep.auto_update_enabled;
                            app.agy_report = Some(rep);
                            if check_auth {
                                app.set_banner(
                                    BadgeTone::Ok,
                                    tr(
                                        app.lang,
                                        "AGY 与认证状态已校验",
                                        "AGY & Auth Status Verified",
                                    ),
                                    tr(
                                        app.lang,
                                        "已完成 Antigravity 在线状态探测",
                                        "Antigravity probe completed",
                                    ),
                                );
                            }
                        }
                        Err(err) => app.set_banner(
                            BadgeTone::Err,
                            tr(app.lang, "检查 AGY 状态失败", "Failed to Inspect AGY"),
                            err.to_string(),
                        ),
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(crate) fn action_agy_install_or_update(
        &mut self,
        check_only: bool,
        force: bool,
        cx: &mut Context<Self>,
    ) {
        if self.busy_agy {
            return;
        }
        self.busy_agy = true;
        cx.notify();

        let state_paths = self.state_paths.clone();
        let installed_already = self.agy_report.as_ref().is_some_and(|r| r.installed);
        let rt = self.rt.clone();
        let (tx, rx) = oneshot::channel();

        rt.spawn(async move {
            let msg_res: Result<String, String> = if installed_already {
                agy_manager::execute_agy_update(
                    &state_paths,
                    None,
                    &AgyUpdateArgs {
                        check: check_only,
                        force,
                        proxy: None,
                        no_restart: false,
                        manifest_base_url: None,
                    },
                )
                .await
                .map(|o| {
                    format!(
                        "status={:?}\ncurrent={}\nlatest={}\nbinary={}\nsha512={}\ndaemon_restarted={}",
                        o.status,
                        o.previous_version.as_deref().unwrap_or("none"),
                        o.latest_version,
                        o.binary_path.display(),
                        o.sha512,
                        o.daemon_restarted
                    )
                })
                .map_err(|e| e.to_string())
            } else {
                agy_manager::execute_agy_install(
                    &state_paths,
                    &AgyInstallArgs {
                        check: check_only,
                        force,
                        dir: None,
                        proxy: None,
                        skip_path: false,
                        manifest_base_url: None,
                    },
                )
                .await
                .map(|o| {
                    format!(
                        "status={:?}\nplatform={}\nversion={}\nbinary={}\nsha512={}\nin_path={}",
                        o.status,
                        o.platform,
                        o.version,
                        o.binary_path.display(),
                        o.sha512,
                        o.dir_in_path
                    )
                })
                .map_err(|e| e.to_string())
            };
            let fresh_status = agy_manager::inspect_agy_status(
                &state_paths,
                None,
                &AgyStatusArgs {
                    offline: false,
                    no_auth_check: true,
                },
            )
            .await
            .ok();
            let _ = tx.send((msg_res, fresh_status));
        });

        cx.spawn(async move |this, cx| {
            if let Ok((msg_res, fresh_status)) = rx.await {
                let _ = this.update(cx, |app, cx| {
                    app.busy_agy = false;
                    if let Some(st) = fresh_status {
                        app.agy_report = Some(st);
                    }
                    match msg_res {
                        Ok(summary) => {
                            app.agy_terminal_output = summary.clone();
                            let first_line = summary.lines().next().unwrap_or("done").to_owned();
                            app.set_banner(
                                BadgeTone::Ok,
                                tr(
                                    app.lang,
                                    "AGY 安装/更新操作完成",
                                    "AGY Install / Update Completed",
                                ),
                                first_line,
                            );
                        }
                        Err(err) => {
                            app.agy_terminal_output = format!("ERROR: {err}");
                            app.set_banner(
                                BadgeTone::Err,
                                tr(app.lang, "AGY 安装/更新失败", "AGY Install / Update Failed"),
                                err,
                            );
                        }
                    }
                    app.refresh_gateway_status(None, cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(crate) fn action_agy_login(&mut self, cx: &mut Context<Self>) {
        if self.busy_agy {
            return;
        }
        self.busy_agy = true;
        cx.notify();

        let state_paths = self.state_paths.clone();
        let rt = self.rt.clone();
        let (tx, rx) = oneshot::channel();

        rt.spawn(async move {
            let res = agy_manager::launch_agy_login_in_terminal(
                &state_paths,
                None,
                &AgyLoginArgs {
                    proxy: None,
                    install: true,
                    no_restart: false,
                },
            )
            .await;
            let _ = tx.send(res);
        });

        cx.spawn(async move |this, cx| {
            if let Ok(res) = rx.await {
                let _ = this.update(cx, |app, cx| {
                    app.busy_agy = false;
                    match res {
                        Ok(bin) => app.set_banner(
                            BadgeTone::Info,
                            tr(
                                app.lang,
                                "已在系统终端拉起 Google 登录授权",
                                "Launched Google OAuth Sign-In in Terminal",
                            ),
                            format!("{} acp --login", bin.display()),
                        ),
                        Err(err) => app.set_banner(
                            BadgeTone::Err,
                            tr(
                                app.lang,
                                "拉起 AGY 登录失败",
                                "Failed to Launch AGY Sign-In",
                            ),
                            err.to_string(),
                        ),
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(crate) fn action_toggle_agy_auto_update(&mut self, enable: bool, cx: &mut Context<Self>) {
        if self.busy_agy {
            return;
        }
        self.busy_agy = true;
        self.cfg_agy_auto_update = enable;
        cx.notify();

        let state_paths = self.state_paths.clone();
        let rt = self.rt.clone();
        let (tx, rx) = oneshot::channel();

        rt.spawn(async move {
            let res = agy_manager::execute_agy_auto_update(
                &state_paths,
                &AgyAutoUpdateArgs {
                    mode: if enable {
                        AutoUpdateToggle::On
                    } else {
                        AutoUpdateToggle::Off
                    },
                    no_restart: false,
                },
            )
            .await;
            let _ = tx.send(res);
        });

        cx.spawn(async move |this, cx| {
            if let Ok(res) = rx.await {
                let _ = this.update(cx, |app, cx| {
                    app.busy_agy = false;
                    match res {
                        Ok(outcome) => {
                            if let Some(rep) = &mut app.agy_report {
                                rep.auto_update_enabled = outcome.agy_auto_update;
                            }
                            app.set_banner(
                                BadgeTone::Ok,
                                tr(
                                    app.lang,
                                    "AGY 闲时自动更新策略已保存",
                                    "Updated AGY Auto-Update Policy",
                                ),
                                format!("auto_update = {}", outcome.agy_auto_update),
                            );
                        }
                        Err(err) => app.set_banner(
                            BadgeTone::Err,
                            tr(
                                app.lang,
                                "更新自动升级设置失败",
                                "Failed to Update Auto-Update",
                            ),
                            err.to_string(),
                        ),
                    }
                    app.refresh_gateway_status(None, cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(crate) fn action_run_agy_subcommand(
        &mut self,
        preset_args: Option<Vec<String>>,
        in_terminal: bool,
        cx: &mut Context<Self>,
    ) {
        if self.busy_agy {
            return;
        }
        let tokens = preset_args.unwrap_or_else(|| {
            let raw = self.in_agy_custom_args.read(cx).value();
            parse_cli_tokens(&raw)
        });
        if tokens.is_empty() {
            return;
        }
        let argv: Vec<OsString> = tokens.iter().map(OsString::from).collect();
        let cmd_display = format!("agy {}", tokens.join(" "));

        if in_terminal {
            match agy_manager::prepare_agy_terminal_spec(&self.state_paths, None, &argv, None, true)
                .and_then(|spec| spawn_in_system_terminal(&self.state_paths, &spec))
            {
                Ok(()) => self.set_banner(
                    BadgeTone::Ok,
                    tr(
                        self.lang,
                        "已在系统终端执行 agy 命令",
                        "Launched agy in Terminal",
                    ),
                    cmd_display,
                ),
                Err(err) => self.set_banner(
                    BadgeTone::Err,
                    tr(self.lang, "执行 agy 失败", "Failed to Run agy"),
                    err.to_string(),
                ),
            }
            cx.notify();
            return;
        }

        self.busy_agy = true;
        cx.notify();
        let state_paths = self.state_paths.clone();
        let rt = self.rt.clone();
        let (tx, rx) = oneshot::channel();

        rt.spawn(async move {
            let res =
                agy_manager::run_agy_captured(&state_paths, None, &argv, Duration::from_secs(20))
                    .await;
            let _ = tx.send(res);
        });

        cx.spawn(async move |this, cx| {
            if let Ok(res) = rx.await {
                let _ = this.update(cx, |app, cx| {
                    app.busy_agy = false;
                    match res {
                        Ok(out) => {
                            let mut combined = format!("$ {cmd_display}\n");
                            if !out.stdout.trim().is_empty() {
                                combined.push_str(out.stdout.trim_end());
                                combined.push('\n');
                            }
                            if !out.stderr.trim().is_empty() {
                                combined.push_str(out.stderr.trim_end());
                                combined.push('\n');
                            }
                            app.agy_terminal_output = combined;
                        }
                        Err(err) => {
                            app.agy_terminal_output = format!("$ {cmd_display}\nERROR: {err}");
                            app.set_banner(
                                BadgeTone::Err,
                                tr(app.lang, "执行 agy 命令失败", "agy Command Failed"),
                                err.to_string(),
                            );
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(crate) fn action_apply_proxy(&mut self, mode: ProxyApplyMode, cx: &mut Context<Self>) {
        if self.busy_proxy {
            return;
        }
        let input_target = self.read_optional_input(&self.in_proxy_target, cx);
        let args = match mode {
            ProxyApplyMode::RefreshOnly => ProxyArgs::default(),
            ProxyApplyMode::ApplyInput => ProxyArgs {
                target: input_target,
                ..ProxyArgs::default()
            },
            ProxyApplyMode::UseSystem => ProxyArgs {
                from_system: true,
                ..ProxyArgs::default()
            },
            ProxyApplyMode::DisableDirect => ProxyArgs {
                off: true,
                ..ProxyArgs::default()
            },
            ProxyApplyMode::TestLatency => ProxyArgs {
                test: true,
                ..ProxyArgs::default()
            },
        };

        self.busy_proxy = true;
        cx.notify();

        let state_paths = self.state_paths.clone();
        let rt = self.rt.clone();
        let (tx, rx) = oneshot::channel();

        rt.spawn(async move {
            let res = agy_manager::execute_proxy(&state_paths, &args).await;
            let _ = tx.send(res);
        });

        cx.spawn(async move |this, cx| {
            if let Ok(res) = rx.await {
                let _ = this.update(cx, |app, cx| {
                    app.busy_proxy = false;
                    match res {
                        Ok(outcome) => {
                            if mode != ProxyApplyMode::RefreshOnly {
                                let summary = match &outcome.connectivity_probe {
                                    Some(p) if p.ok => {
                                        let latest_v =
                                            p.latest_agy_version.as_deref().unwrap_or("unknown");
                                        if app.lang.is_en() {
                                            format!(
                                                "Latency {}ms · latest agy {latest_v}",
                                                p.latency_ms
                                            )
                                        } else {
                                            format!(
                                                "连通延迟 {}ms · 最新 agy {latest_v}",
                                                p.latency_ms
                                            )
                                        }
                                    }
                                    Some(p) => {
                                        let err_s = p.error.as_deref().unwrap_or("error");
                                        if app.lang.is_en() {
                                            format!("Probe failed ({}ms): {err_s}", p.latency_ms)
                                        } else {
                                            format!("连通性探测失败（{}ms）：{err_s}", p.latency_ms)
                                        }
                                    }
                                    None => outcome.effective_proxy.as_ref().map_or_else(
                                        || {
                                            tr(app.lang, "直连（未启用代理）", "direct (no proxy)")
                                                .to_owned()
                                        },
                                        |p| p.redacted_display().into_owned(),
                                    ),
                                };
                                let tone =
                                    if outcome.connectivity_probe.as_ref().is_some_and(|p| !p.ok) {
                                        BadgeTone::Err
                                    } else {
                                        BadgeTone::Ok
                                    };
                                app.set_banner(
                                    tone,
                                    tr(
                                        app.lang,
                                        "代理配置与连通性已更新",
                                        "Proxy & Connectivity Updated",
                                    ),
                                    summary,
                                );
                            }
                            app.proxy_outcome = Some(outcome);
                        }
                        Err(err) => {
                            if mode != ProxyApplyMode::RefreshOnly {
                                app.set_banner(
                                    BadgeTone::Err,
                                    tr(app.lang, "代理设置失败", "Proxy Operation Failed"),
                                    err.to_string(),
                                );
                            }
                        }
                    }
                    app.refresh_gateway_status(None, cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    // =========================================================================
    // Logs, Doctor, Alias, Completions & Self-Upgrade
    // =========================================================================

    pub(crate) fn refresh_logs(&mut self, cx: &mut Context<Self>) {
        if let Ok(tail) = daemon::read_log_tail(&self.state_paths.log_file(), self.log_lines_limit)
        {
            self.log_tail = tail;
            cx.notify();
        }
    }

    pub(crate) fn action_clear_logs(&mut self, cx: &mut Context<Self>) {
        match daemon::clear_gateway_log(&self.state_paths) {
            Ok(path) => {
                self.log_tail.clear();
                self.set_banner(
                    BadgeTone::Ok,
                    tr(self.lang, "网关日志已清空", "Gateway Log Cleared"),
                    path.display().to_string(),
                );
            }
            Err(err) => self.set_banner(
                BadgeTone::Err,
                tr(self.lang, "清空日志失败", "Failed to Clear Log"),
                err.to_string(),
            ),
        }
        cx.notify();
    }

    pub(crate) fn action_open_state_dir(&mut self, cx: &mut Context<Self>) {
        let root = self.state_paths.root().to_path_buf();
        let opener = if cfg!(windows) {
            "explorer.exe"
        } else if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        match std::process::Command::new(opener).arg(&root).spawn() {
            Ok(_) => self.set_banner(
                BadgeTone::Info,
                tr(self.lang, "已打开状态与日志目录", "Opened State Directory"),
                root.display().to_string(),
            ),
            Err(err) => self.set_banner(
                BadgeTone::Err,
                tr(self.lang, "打开目录失败", "Failed to Open Directory"),
                err.to_string(),
            ),
        }
        cx.notify();
    }

    pub(crate) fn action_run_doctor(&mut self, cx: &mut Context<Self>) {
        if self.busy_doctor {
            return;
        }
        self.busy_doctor = true;
        cx.notify();

        let state_paths = self.state_paths.clone();
        let listen = self.parse_listen_override(cx);
        let api_key = self.read_optional_input(&self.in_api_key, cx);
        let rt = self.rt.clone();
        let (tx, rx) = oneshot::channel();

        rt.spawn(async move {
            let res = daemon::inspect_doctor(
                &state_paths,
                &DoctorArgs {
                    listen,
                    api_key,
                    agy_bin: None,
                    json: false,
                },
                "lg",
            )
            .await;
            let _ = tx.send(res);
        });

        cx.spawn(async move |this, cx| {
            if let Ok(res) = rx.await {
                let _ = this.update(cx, |app, cx| {
                    app.busy_doctor = false;
                    match res {
                        Ok(rep) => {
                            app.doctor_report = Some(rep);
                        }
                        Err(err) => app.set_banner(
                            BadgeTone::Err,
                            tr(app.lang, "环境诊断失败", "Doctor Check Failed"),
                            err.to_string(),
                        ),
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(crate) fn refresh_alias_and_completions(&mut self, cx: &mut Context<Self>) {
        let alias_name = self
            .read_optional_input(&self.in_alias_name, cx)
            .unwrap_or_else(|| "lg".to_owned());
        self.alias_report = daemon::inspect_alias_status(&alias_name, None).ok();
        self.completions_preview = render_completions_text(self.selected_shell, &alias_name);
    }

    pub(crate) fn action_manage_alias(&mut self, remove: bool, cx: &mut Context<Self>) {
        let name = self
            .read_optional_input(&self.in_alias_name, cx)
            .unwrap_or_else(|| "lg".to_owned());
        let args = AliasArgs {
            name: name.clone(),
            dir: None,
            force: true,
            uninstall: remove,
        };
        match daemon::execute_alias(&args) {
            Ok(outcome) => {
                self.set_banner(
                    BadgeTone::Ok,
                    tr(self.lang, "CLI 别名操作已完成", "CLI Alias Updated"),
                    format!("{:?} -> {}", outcome.status, outcome.target_path.display()),
                );
            }
            Err(err) => {
                self.set_banner(
                    BadgeTone::Err,
                    tr(self.lang, "CLI 别名设置失败", "CLI Alias Operation Failed"),
                    err.to_string(),
                );
            }
        }
        self.refresh_alias_and_completions(cx);
        cx.notify();
    }

    pub(crate) fn action_run_upgrade(&mut self, check_only: bool, cx: &mut Context<Self>) {
        if self.busy_upgrade {
            return;
        }
        self.busy_upgrade = true;
        cx.notify();

        let state_paths = self.state_paths.clone();
        let current_exe = self.current_exe.clone();
        let version = self.read_optional_input(&self.in_upgrade_version, cx);
        let force = self.upgrade_force;
        let rt = self.rt.clone();
        let (tx, rx) = oneshot::channel();

        rt.spawn(async move {
            let args = UpgradeArgs {
                check: check_only,
                force,
                version,
                ..Default::default()
            };
            let res = upgrade::execute_upgrade(&state_paths, &args, &current_exe).await;
            let _ = tx.send(res);
        });

        cx.spawn(async move |this, cx| {
            if let Ok(res) = rx.await {
                let _ = this.update(cx, |app, cx| {
                    app.busy_upgrade = false;
                    match res {
                        Ok(outcome) => {
                            let detail = format!(
                                "v{} -> {} ({})",
                                outcome.current_version, outcome.tag, outcome.status
                            );
                            let tone = if outcome.update_available || outcome.upgraded {
                                BadgeTone::Ok
                            } else {
                                BadgeTone::Info
                            };
                            app.upgrade_outcome = Some(outcome);
                            app.set_banner(
                                tone,
                                tr(app.lang, "自升级检查/安装完成", "Self-Upgrade Completed"),
                                detail,
                            );
                        }
                        Err(err) => app.set_banner(
                            BadgeTone::Err,
                            tr(app.lang, "自升级失败", "Self-Upgrade Failed"),
                            err.to_string(),
                        ),
                    }
                    app.refresh_gateway_status(None, cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    // =========================================================================
    // Top Chrome + Sidebar + View Shell
    // =========================================================================

    fn render_top_bar(&self, p: &Palette, cx: &mut Context<Self>) -> Div {
        let lang = self.lang;
        let (gw_label, gw_tone) = match self.gateway_report.as_ref().map(|r| r.mode) {
            Some(GatewayStatusMode::Managed) => (
                tr(lang, "网关在线 · 托管", "ONLINE · MANAGED"),
                BadgeTone::Ok,
            ),
            Some(GatewayStatusMode::External) => (
                tr(lang, "网关在线 · 外部", "ONLINE · EXTERNAL"),
                BadgeTone::Info,
            ),
            _ => (tr(lang, "网关离线", "OFFLINE"), BadgeTone::Neutral),
        };

        let sse_badge = if self.sse_connected {
            pill_badge(p, BadgeTone::Primary, tr(lang, "SSE 实时流", "SSE LIVE"))
        } else {
            pill_badge(p, BadgeTone::Neutral, tr(lang, "SSE 待命", "SSE IDLE"))
        };

        let lang_btn_label = if self.lang_pref.is_system() {
            match lang {
                DesktopLang::Zh => "中文 · 自动",
                DesktopLang::En => "EN · Auto",
            }
        } else {
            match lang {
                DesktopLang::Zh => "中文 / EN",
                DesktopLang::En => "EN / 中文",
            }
        };
        let theme_btn_label = if self.theme_pref.is_system() {
            if self.dark_mode {
                tr(lang, "深色 · 自动", "Dark · Auto")
            } else {
                tr(lang, "浅色 · 自动", "Light · Auto")
            }
        } else if self.dark_mode {
            tr(lang, "深色墨韵", "Warm Dark")
        } else {
            tr(lang, "浅色暖纸", "Warm Light")
        };
        let has_manual_override = !self.lang_pref.is_system() || !self.theme_pref.is_system();

        let listen_url = self.gateway_report.as_ref().map_or_else(
            || "http://127.0.0.1:8787".to_owned(),
            |r| r.gateway_url.clone(),
        );

        div()
            .w_full()
            .h(px(56.0))
            .px_5()
            .bg(p.surface)
            .border_b_1()
            .border_color(p.border)
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .size(px(28.0))
                            .rounded(px(7.0))
                            .bg(p.primary)
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_sm()
                            .font_weight(FontWeight::BOLD)
                            .text_color(p.on_primary)
                            .child("L"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(p.text)
                                            .child("Liftgate"),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(p.faint)
                                            .child(concat!("v", env!("CARGO_PKG_VERSION"))),
                                    ),
                            )
                            .child(div().text_xs().text_color(p.muted).child(tr(
                                lang,
                                "Antigravity ACP 本地网关与 AI 工作台",
                                "Antigravity ACP Local Gateway & AI Control Center",
                            ))),
                    )
                    .child(div().ml_2().child(pill_badge(p, gw_tone, gw_label)))
                    .child(sse_badge),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child({
                        let url_copy = listen_url.clone();
                        Button::new("hdr-copy-url")
                            .small()
                            .ghost()
                            .icon(IconName::Copy)
                            .label(listen_url)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.copy_text(
                                    tr(this.lang, "网关地址", "Gateway URL"),
                                    url_copy.clone(),
                                    cx,
                                );
                            }))
                    })
                    .child(
                        Button::new("hdr-toggle-lang")
                            .small()
                            .outline()
                            .icon(IconName::Globe)
                            .label(lang_btn_label)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_language(window, cx);
                            })),
                    )
                    .child(
                        Button::new("hdr-toggle-theme")
                            .small()
                            .outline()
                            .icon(if self.dark_mode {
                                IconName::Sun
                            } else {
                                IconName::Moon
                            })
                            .label(theme_btn_label)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_dark_mode(cx);
                            })),
                    )
                    .children(has_manual_override.then(|| {
                        Button::new("hdr-follow-os")
                            .small()
                            .ghost()
                            .icon(IconName::RefreshCw)
                            .label(tr(lang, "跟随系统", "Follow OS"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.reset_prefs_to_system(window, cx);
                            }))
                    })),
            )
    }

    fn render_sidebar(&self, p: &Palette, cx: &mut Context<Self>) -> Div {
        let lang = self.lang;
        let active_sessions = self.live_telemetry.as_ref().map_or(0, |t| {
            t.session_pool.active_leased_sessions + t.session_pool.idle_cached_sessions
        });
        let active_turns = self
            .live_telemetry
            .as_ref()
            .map_or(0, |t| t.active_tasks.len());
        let total_tokens = self.live_telemetry.as_ref().map_or(0, |t| {
            t.metrics.total_input_tokens
                + t.metrics.total_output_tokens
                + t.metrics.total_thought_tokens
        });

        div()
            .w(px(238.0))
            .flex_shrink_0()
            .h_full()
            .bg(p.surface)
            .border_r_1()
            .border_color(p.border)
            .p_3()
            .flex()
            .flex_col()
            .justify_between()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .px_2p5()
                            .py_1p5()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.faint)
                            .child(tr(lang, "工作台导航", "NAVIGATION")),
                    )
                    .children(NavTab::ALL.into_iter().map(|tab| {
                        let selected = self.active_tab == tab;
                        let (bg_c, fg_c, idx_c, border_c) = if selected {
                            (
                                p.primary_soft,
                                p.primary,
                                p.primary,
                                p.primary.opacity(0.35),
                            )
                        } else {
                            (
                                gpui_kit::gpui::transparent_black(),
                                p.text_2,
                                p.faint,
                                gpui_kit::gpui::transparent_black(),
                            )
                        };
                        div()
                            .id(SharedString::from(format!("nav-{}", tab.index_tag())))
                            .cursor_pointer()
                            .px_2p5()
                            .py_2()
                            .rounded(px(8.0))
                            .bg(bg_c)
                            .border_1()
                            .border_color(border_c)
                            .hover(|s| if selected { s } else { s.bg(p.surface_alt) })
                            .flex()
                            .items_center()
                            .justify_between()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.select_tab(tab, cx);
                            }))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2p5()
                                    .child(Icon::new(tab.icon()).small().text_color(fg_c))
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(if selected {
                                                FontWeight::SEMIBOLD
                                            } else {
                                                FontWeight::MEDIUM
                                            })
                                            .text_color(fg_c)
                                            .child(tab.label(lang)),
                                    ),
                            )
                            .child(div().text_xs().text_color(idx_c).child(tab.index_tag()))
                    })),
            )
            .child(
                div()
                    .p_3()
                    .rounded(px(9.0))
                    .bg(p.surface_alt)
                    .border_1()
                    .border_color(p.border)
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.primary)
                            .child(tr(lang, "实时脉搏", "LIVE PULSE")),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .text_xs()
                            .child(div().text_color(p.muted).child(tr(
                                lang,
                                "活跃推理轮次",
                                "Active Turns",
                            )))
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(p.text)
                                    .child(active_turns.to_string()),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .text_xs()
                            .child(div().text_color(p.muted).child(tr(
                                lang,
                                "驻留热会话池",
                                "Warm Sessions",
                            )))
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(p.text)
                                    .child(active_sessions.to_string()),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .text_xs()
                            .child(div().text_color(p.muted).child(tr(
                                lang,
                                "累计吞吐 Tokens",
                                "Total Tokens",
                            )))
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(p.text)
                                    .child(format_tokens(total_tokens)),
                            ),
                    ),
            )
    }

    fn render_banner(&self, p: &Palette, cx: &mut Context<Self>) -> Option<Div> {
        let b = self.banner.as_ref()?;
        let (bg_c, fg_c, border_c) = b.tone.colors(p);
        Some(
            div()
                .w_full()
                .px_3p5()
                .py_2()
                .rounded(px(8.0))
                .bg(bg_c)
                .border_1()
                .border_color(border_c.opacity(0.45))
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2p5()
                        .child(div().size(px(7.0)).rounded(px(999.0)).bg(fg_c))
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(fg_c)
                                .child(b.title.clone()),
                        )
                        .child(div().text_xs().text_color(p.text_2).child(b.detail.clone())),
                )
                .child(
                    Button::new("dismiss-banner")
                        .xsmall()
                        .ghost()
                        .icon(IconName::Close)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.banner = None;
                            cx.notify();
                        })),
                ),
        )
    }
}

impl Render for DesktopApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.palette();
        let lang = self.lang;
        let tab = self.active_tab;

        let page_header = div()
            .w_full()
            .pb_2()
            .border_b_1()
            .border_color(p.border)
            .flex()
            .items_end()
            .justify_between()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.primary)
                            .child(tab.eyebrow(lang)),
                    )
                    .child(
                        div()
                            .text_xl()
                            .font_weight(FontWeight::BOLD)
                            .text_color(p.text)
                            .child(tab.label(lang)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(p.muted)
                            .child(tab.subtitle(lang)),
                    ),
            )
            .child(
                Button::new("page-refresh")
                    .small()
                    .outline()
                    .icon(IconName::RefreshCw)
                    .label(tr(lang, "刷新当前视图", "Refresh"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.refresh_gateway_status(Some(window), cx);
                        let cur = this.active_tab;
                        this.select_tab(cur, cx);
                    })),
            );

        let tab_body = match tab {
            NavTab::Overview => self.render_overview_tab(&p, window, cx),
            NavTab::Telemetry => self.render_telemetry_tab(&p, cx),
            NavTab::Launchers => self.render_launchers_tab(&p, cx),
            NavTab::Models => self.render_models_tab(&p, window, cx),
            NavTab::AgyProxy => self.render_agy_proxy_tab(&p, window, cx),
            NavTab::System => self.render_system_tab(&p, cx),
        };

        let main_canvas = div()
            .id("main-editorial-canvas")
            .flex_1()
            .h_full()
            .p_5()
            .flex()
            .flex_col()
            .gap_4()
            .children(self.render_banner(&p, cx))
            .child(page_header)
            .child(tab_body)
            .overflow_y_scrollbar();

        div()
            .size_full()
            .bg(p.bg)
            .text_color(p.text)
            .flex()
            .flex_col()
            .child(self.render_top_bar(&p, cx))
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .overflow_hidden()
                    .flex()
                    .flex_row()
                    .child(self.render_sidebar(&p, cx))
                    .child(main_canvas),
            )
    }
}

/// Splits a CLI passthrough input string into shell-like tokens while honoring single and double quotes.
///
/// Preserves literal backslashes inside double quotes and unquoted Windows paths (only treating
/// backslashes before `"` via standard `2n` / `2n + 1` rules, plus `\ ` and `\'` outside quotes)
/// so paths like `-p "read C:\repo\src\main.rs"` reach target programs intact.
#[must_use]
pub fn parse_cli_tokens(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = raw.chars().peekable();

    while let Some(ch) = chars.next() {
        if in_single {
            if ch == '\'' {
                in_single = false;
            } else {
                current.push(ch);
            }
            continue;
        }

        if in_double {
            match ch {
                '"' => in_double = false,
                '\\' => {
                    let mut slashes = 1usize;
                    while chars.next_if_eq(&'\\').is_some() {
                        slashes += 1;
                    }
                    if chars.next_if_eq(&'"').is_some() {
                        for _ in 0..(slashes / 2) {
                            current.push('\\');
                        }
                        if slashes % 2 == 1 {
                            current.push('"');
                        } else {
                            in_double = false;
                        }
                    } else {
                        for _ in 0..slashes {
                            current.push('\\');
                        }
                    }
                }
                other => current.push(other),
            }
            continue;
        }

        match ch {
            c if c.is_whitespace() => {
                if has_token {
                    out.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            '\'' => {
                in_single = true;
                has_token = true;
            }
            '"' => {
                in_double = true;
                has_token = true;
            }
            '\\' => {
                has_token = true;
                let mut slashes = 1usize;
                while chars.next_if_eq(&'\\').is_some() {
                    slashes += 1;
                }
                if chars.next_if_eq(&'"').is_some() {
                    for _ in 0..(slashes / 2) {
                        current.push('\\');
                    }
                    if slashes % 2 == 1 {
                        current.push('"');
                    } else {
                        in_double = true;
                    }
                } else if slashes == 1
                    && chars
                        .peek()
                        .is_some_and(|next| next.is_whitespace() || *next == '\'')
                {
                    if let Some(escaped) = chars.next() {
                        current.push(escaped);
                    }
                } else {
                    for _ in 0..slashes {
                        current.push('\\');
                    }
                }
            }
            other => {
                current.push(other);
                has_token = true;
            }
        }
    }
    if has_token {
        out.push(current);
    }
    out
}

/// Generates shell completion script text for previewing and copying inside the desktop UI.
#[must_use]
pub fn render_completions_text(shell: Shell, bin_name: &str) -> String {
    let mut cmd = Cli::command();
    let mut buf = Vec::with_capacity(4096);
    clap_complete::generate(shell, &mut cmd, bin_name, &mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

#[cfg(test)]
mod tests {
    use super::parse_cli_tokens;

    #[test]
    fn splits_quoted_and_escaped_passthrough_arguments() {
        let parsed = parse_cli_tokens("-p \"review auth diff\" --model 'gemini-4-carbon-high'");
        assert_eq!(
            parsed,
            vec!["-p", "review auth diff", "--model", "gemini-4-carbon-high"]
        );
    }

    #[test]
    fn preserves_windows_backslashes_and_escaped_quotes() {
        assert_eq!(
            parse_cli_tokens(r#"-p "read C:\repo\src\main.rs""#),
            vec!["-p", r"read C:\repo\src\main.rs"]
        );
        assert_eq!(
            parse_cli_tokens(r#"-p "say \"hi\" in C:\new\temp\\""#),
            vec!["-p", r#"say "hi" in C:\new\temp\"#]
        );
        assert_eq!(
            parse_cli_tokens(r"-C C:\repo\src --dir /tmp/my\ folder -p ''"),
            vec!["-C", r"C:\repo\src", "--dir", "/tmp/my folder", "-p", ""]
        );
    }
}
