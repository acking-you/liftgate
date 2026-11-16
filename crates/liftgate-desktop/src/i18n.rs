//! Bilingual (`中文` / `EN`) copy helper and navigation metadata for `liftgate-desktop`.

use gpui_kit::component::IconName;
use liftgate_core::state::DesktopLang;

/// Returns the localized string matching `lang`.
#[must_use]
pub const fn tr(lang: DesktopLang, zh: &'static str, en: &'static str) -> &'static str {
    match lang {
        DesktopLang::En => en,
        DesktopLang::Zh => zh,
    }
}

/// Localized main native window title.
#[must_use]
pub const fn window_title(lang: DesktopLang) -> &'static str {
    tr(
        lang,
        "Liftgate · Antigravity 桌面控制中心",
        "Liftgate · Antigravity Desktop Control Center",
    )
}

/// Primary sidebar tabs covering 100% of Liftgate CLI capabilities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavTab {
    Overview,
    Telemetry,
    Launchers,
    Models,
    AgyProxy,
    System,
}

impl NavTab {
    pub const ALL: [Self; 6] = [
        Self::Overview,
        Self::Telemetry,
        Self::Launchers,
        Self::Models,
        Self::AgyProxy,
        Self::System,
    ];

    #[must_use]
    pub const fn index_tag(self) -> &'static str {
        match self {
            Self::Overview => "01",
            Self::Telemetry => "02",
            Self::Launchers => "03",
            Self::Models => "04",
            Self::AgyProxy => "05",
            Self::System => "06",
        }
    }

    #[must_use]
    pub const fn icon(self) -> IconName {
        match self {
            Self::Overview => IconName::LayoutDashboard,
            Self::Telemetry => IconName::ChartPie,
            Self::Launchers => IconName::Play,
            Self::Models => IconName::Bot,
            Self::AgyProxy => IconName::Network,
            Self::System => IconName::SquareTerminal,
        }
    }

    #[must_use]
    pub const fn label(self, lang: DesktopLang) -> &'static str {
        match self {
            Self::Overview => tr(lang, "概览与网关", "Overview & Gateway"),
            Self::Telemetry => tr(lang, "实时监控与会话", "Telemetry & Sessions"),
            Self::Launchers => tr(lang, "启动器与配置同步", "Launchers & Config"),
            Self::Models => tr(lang, "模型目录", "Models Catalog"),
            Self::AgyProxy => tr(lang, "AGY 与网络代理", "AGY & Network Proxy"),
            Self::System => tr(lang, "日志 · 诊断 · 系统", "Logs · Doctor · System"),
        }
    }

    #[must_use]
    pub const fn subtitle(self, lang: DesktopLang) -> &'static str {
        match self {
            Self::Overview => tr(
                lang,
                "本地守护进程生命周期、监听参数与快速入口",
                "Daemon lifecycle, serve parameters & quick workbench",
            ),
            Self::Telemetry => tr(
                lang,
                "SSE 实时事件流、热会话池管理与单轮工具追踪",
                "Live SSE stream, warm session pool & turn inspector",
            ),
            Self::Launchers => tr(
                lang,
                "零污染拉起 Claude / Codex 终端、桌面端与配置同步",
                "Zero-pollution CLI/GUI launchers & config sync",
            ),
            Self::Models => tr(
                lang,
                "实时 /v1/models 探索、上下文窗口与默认模型切换",
                "Live /v1/models catalog, context tiers & default switcher",
            ),
            Self::AgyProxy => tr(
                lang,
                "Antigravity 安装更新、Google 认证与强制代理",
                "Antigravity installer, Google auth & forced proxy",
            ),
            Self::System => tr(
                lang,
                "网关日志追踪、Doctor 诊断、别名补全与校验自升级",
                "Gateway logs, environment doctor, alias & self-upgrade",
            ),
        }
    }

    #[must_use]
    pub const fn eyebrow(self, lang: DesktopLang) -> &'static str {
        match self {
            Self::Overview => tr(lang, "01 · 网关控制中心", "01 · GATEWAY CONTROL"),
            Self::Telemetry => tr(
                lang,
                "02 · 实时遥测与热会话池",
                "02 · LIVE TELEMETRY & SESSIONS",
            ),
            Self::Launchers => tr(
                lang,
                "03 · AI 启动台与配置同步",
                "03 · AI WORKBENCH & CONFIG SYNC",
            ),
            Self::Models => tr(
                lang,
                "04 · 上游模型目录与路由",
                "04 · ANTIGRAVITY MODEL CATALOG",
            ),
            Self::AgyProxy => tr(
                lang,
                "05 · AGY 引擎与网络代理",
                "05 · AGY LIFECYCLE & NETWORK PROXY",
            ),
            Self::System => tr(
                lang,
                "06 · 日志 · 诊断 · 系统工具",
                "06 · LOGS · DOCTOR · SYSTEM",
            ),
        }
    }
}
