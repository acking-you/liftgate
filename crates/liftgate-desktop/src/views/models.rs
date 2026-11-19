//! Tab 04: Live `/v1/models` Catalog & Default Model Switcher (`模型目录`).

use gpui_kit::base::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::{IconName, Sizable as _};
use gpui_kit::*;
use liftgate_core::format::format_tokens;

use crate::app::{DesktopApp, ModelFamilyFilter};
use crate::i18n::tr;
use crate::theme::{BadgeTone, Palette, editorial_card, kpi_card, pill_badge, section_header};

impl DesktopApp {
    pub(crate) fn render_models_tab(
        &self,
        p: &Palette,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let lang = self.lang;
        let search_q = self
            .in_model_search
            .read(cx)
            .value()
            .trim()
            .to_ascii_lowercase();
        let family = self.model_family;

        let Some(report) = &self.models_report else {
            return editorial_card(p)
                .items_start()
                .child(section_header(
                    p,
                    tr(lang, "上游模型目录", "MODEL CATALOG"),
                    tr(lang, "尚未加载上游模型目录", "Model Catalog Not Loaded Yet"),
                    Some(tr(
                        lang,
                        "点击下方按钮将自动连接（或拉起）本地网关，实时拉取 `/v1/models` 可用模型及上下文窗口策略。",
                        "Click below to query `/v1/models` (automatically starting the local daemon if offline).",
                    )),
                ))
                .child(
                    Button::new("mdl-discover-now")
                        .small()
                        .primary()
                        .loading(self.busy_models)
                        .icon(IconName::Star)
                        .label(tr(lang, "立即拉取模型目录", "Discover Available Models"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.action_refresh_models(true, cx);
                        })),
                );
        };

        let filtered: Vec<_> = report
            .models
            .iter()
            .filter(|m| {
                family.matches(&m.id)
                    && (search_q.is_empty()
                        || m.id.to_ascii_lowercase().contains(&search_q)
                        || m.display_name.to_ascii_lowercase().contains(&search_q))
            })
            .collect();

        let summary_row = div()
            .w_full()
            .flex()
            .flex_row()
            .gap_3()
            .child(kpi_card(
                p,
                tr(lang, "可用模型总数", "DISCOVERED MODELS"),
                format!("{} / {}", filtered.len(), report.models.len()),
                format!(
                    "{}/v1/models · {}",
                    report.gateway_url,
                    tr(lang, "500k 标准窗口", "500k Standard")
                ),
            ))
            .child(kpi_card(
                p,
                tr(lang, "当前默认主模型", "PRIMARY DEFAULT MODEL"),
                report.default_model.clone(),
                tr(
                    lang,
                    "未显式指定模型时自动路由至此模型",
                    "Used when client omits explicit model",
                ),
            ))
            .child(kpi_card(
                p,
                tr(lang, "会话标题模型", "SESSION TITLE MODEL"),
                report.title_model.clone(),
                tr(
                    lang,
                    "生成 Codex / Claude 会话标题（实时生效）",
                    "Generates Codex & Claude session titles",
                ),
            ))
            .child(kpi_card(
                p,
                tr(lang, "轻量辅助模型", "FAST / AUXILIARY MODEL"),
                report.fast_model.clone(),
                tr(
                    lang,
                    "处理分类与其他轻量辅助请求",
                    "Handles classification and other lightweight requests",
                ),
            ));

        let toolbar_card = editorial_card(p).child(
            div()
                .w_full()
                .flex()
                .flex_wrap()
                .items_center()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(240.0))
                        .child(Input::new(&self.in_model_search).small().cleanable(true)),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_1p5()
                        .children(ModelFamilyFilter::ALL.into_iter().map(|fam| {
                            let active = self.model_family == fam;
                            let b = Button::new(SharedString::from(format!("mf-{fam:?}")))
                                .xsmall()
                                .label(fam.label(lang));
                            let styled = if active { b.primary() } else { b.outline() };
                            styled.on_click(cx.listener(move |this, _, _, cx| {
                                this.model_family = fam;
                                cx.notify();
                            }))
                        }))
                        .child(
                            Button::new("mdl-refresh-btn")
                                .small()
                                .secondary()
                                .loading(self.busy_models)
                                .icon(IconName::RefreshCw)
                                .label(tr(lang, "刷新目录", "Refresh"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.action_refresh_models(true, cx);
                                })),
                        ),
                ),
        );

        let list_card = editorial_card(p)
            .child(section_header(
                p,
                tr(lang, "上游 ANTIGRAVITY 模型", "UPSTREAM ANTIGRAVITY MODELS"),
                tr(
                    lang,
                    "模型列表与一键切换 / 启动",
                    "Available Models & Quick Actions",
                ),
                None,
            ))
            .child(if filtered.is_empty() {
                div().py_4().text_xs().text_color(p.muted).child(tr(
                    lang,
                    "没有匹配当前筛选条件的模型。",
                    "No models match the current filter.",
                ))
            } else {
                div().w_full().flex().flex_col().gap_2().children(
                    filtered.into_iter().enumerate().map(|(idx, m)| {
                        let is_default = m.id == report.default_model;
                        let is_fast = m.id == report.fast_model;
                        let is_title = m.id == report.title_model;
                        let (ctx_win, compact_lim) = m.context_limits();
                        let id_copy = m.id.clone();
                        let id_def = m.id.clone();
                        let id_title = m.id.clone();
                        let id_claude = m.id.clone();
                        let id_codex = m.id.clone();
                        let display_title = if m.display_name.trim().is_empty() {
                            m.id.clone()
                        } else {
                            m.display_name.clone()
                        };

                        div()
                            .w_full()
                            .p_3p5()
                            .rounded(px(8.0))
                            .bg(if is_default {
                                p.primary_soft
                            } else {
                                p.surface_alt
                            })
                            .border_1()
                            .border_color(if is_default { p.primary } else { p.border })
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .flex()
                                            .flex_wrap()
                                            .items_center()
                                            .gap_2()
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .text_color(p.text)
                                                    .child(display_title),
                                            )
                                            .children(is_default.then(|| {
                                                pill_badge(
                                                    p,
                                                    BadgeTone::Brand,
                                                    tr(lang, "默认主模型", "DEFAULT"),
                                                )
                                            }))
                                            .children(is_title.then(|| {
                                                pill_badge(
                                                    p,
                                                    BadgeTone::Info,
                                                    tr(lang, "会话标题", "TITLE"),
                                                )
                                            }))
                                            .children(is_fast.then(|| {
                                                pill_badge(
                                                    p,
                                                    BadgeTone::Ok,
                                                    tr(lang, "轻量辅助", "FAST / AUX"),
                                                )
                                            }))
                                            .child(pill_badge(
                                                p,
                                                BadgeTone::Neutral,
                                                if lang.is_en() {
                                                    format!(
                                                        "ctx {} · compact {}",
                                                        format_tokens(ctx_win),
                                                        format_tokens(compact_lim)
                                                    )
                                                } else {
                                                    format!(
                                                        "上下文 {} · 压缩阈值 {}",
                                                        format_tokens(ctx_win),
                                                        format_tokens(compact_lim)
                                                    )
                                                },
                                            )),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(p.muted)
                                            .child(format!("id: {}", m.id)),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .items_center()
                                    .gap_1p5()
                                    .child(
                                        Button::new(SharedString::from(format!("mdl-cp-{idx}")))
                                            .xsmall()
                                            .ghost()
                                            .icon(IconName::Copy)
                                            .label(tr(lang, "复制 ID", "Copy ID"))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.copy_text(
                                                    tr(this.lang, "模型 ID", "Model ID"),
                                                    id_copy.clone(),
                                                    cx,
                                                );
                                            })),
                                    )
                                    .child(
                                        Button::new(SharedString::from(format!("mdl-def-{idx}")))
                                            .xsmall()
                                            .primary()
                                            .disabled(is_default || self.busy_gateway)
                                            .icon(IconName::Check)
                                            .label(tr(lang, "设为默认", "Set Default"))
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.action_set_default_model(
                                                    id_def.clone(),
                                                    window,
                                                    cx,
                                                );
                                            })),
                                    )
                                    .child(
                                        Button::new(SharedString::from(format!("mdl-ttl-{idx}")))
                                            .xsmall()
                                            .secondary()
                                            .disabled(is_title)
                                            .icon(IconName::Star)
                                            .label(tr(lang, "设为标题模型", "Set Title"))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.action_set_title_model(id_title.clone(), cx);
                                            })),
                                    )
                                    .child(
                                        Button::new(SharedString::from(format!("mdl-cl-{idx}")))
                                            .xsmall()
                                            .outline()
                                            .icon(IconName::SquareTerminal)
                                            .label("Claude CLI")
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.action_launch_claude_cli(
                                                    Some(id_claude.clone()),
                                                    false,
                                                    cx,
                                                );
                                            })),
                                    )
                                    .child(
                                        Button::new(SharedString::from(format!("mdl-cx-{idx}")))
                                            .xsmall()
                                            .outline()
                                            .icon(IconName::Cpu)
                                            .label("Codex CLI")
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.action_launch_codex_cli(
                                                    Some(id_codex.clone()),
                                                    false,
                                                    cx,
                                                );
                                            })),
                                    ),
                            )
                    }),
                )
            });

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_4()
            .child(summary_row)
            .child(toolbar_card)
            .child(list_card)
    }
}
