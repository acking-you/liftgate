//! Warm Editorial / StaticFlow (`https://ackingliu.top`) design tokens, GPUI theme bridge,
//! and reusable editorial UI primitives.

use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::theme::{Theme, ThemeMode};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use liftgate_core::ReportTone;

/// Complete StaticFlow (`ackingliu.top`) color palette for Light and Dark editorial modes.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub bg: Hsla,
    pub surface: Hsla,
    pub surface_alt: Hsla,
    pub code_bg: Hsla,
    pub text: Hsla,
    pub text_2: Hsla,
    pub muted: Hsla,
    pub faint: Hsla,
    pub border: Hsla,
    pub border_strong: Hsla,
    pub primary: Hsla,
    pub primary_hover: Hsla,
    pub primary_soft: Hsla,
    pub on_primary: Hsla,
    pub ok: Hsla,
    pub ok_soft: Hsla,
    pub warn: Hsla,
    pub warn_soft: Hsla,
    pub err: Hsla,
    pub err_soft: Hsla,
    pub info: Hsla,
    pub info_soft: Hsla,
    pub term_bg: Hsla,
    pub term_header: Hsla,
    pub term_fg: Hsla,
    pub term_muted: Hsla,
    pub term_border: Hsla,
}

impl Palette {
    #[must_use]
    pub fn for_mode(dark: bool) -> Self {
        if dark { Self::dark() } else { Self::light() }
    }

    /// Default Warm Editorial light mode (`#faf9f5` warm paper + `#c96442` terracotta).
    #[must_use]
    pub fn light() -> Self {
        Self {
            bg: rgb(0xfaf9f5).into(),
            surface: rgb(0xffffff).into(),
            surface_alt: rgb(0xf4f1ea).into(),
            code_bg: rgb(0xf0eee6).into(),
            text: rgb(0x141413).into(),
            text_2: rgb(0x3d3d3a).into(),
            muted: rgb(0x5e5d59).into(),
            faint: rgb(0x87867f).into(),
            border: rgb(0xe8e6df).into(),
            border_strong: rgb(0xdad7cd).into(),
            primary: rgb(0xc96442).into(),
            primary_hover: rgb(0xb5573a).into(),
            primary_soft: rgb(0xf4e4dc).into(),
            on_primary: rgb(0xffffff).into(),
            ok: rgb(0x5a7a4f).into(),
            ok_soft: rgb(0xe6eddf).into(),
            warn: rgb(0xb7862f).into(),
            warn_soft: rgb(0xf5ecd9).into(),
            err: rgb(0xb5443a).into(),
            err_soft: rgb(0xf5deda).into(),
            info: rgb(0x4a6fa5).into(),
            info_soft: rgb(0xe3ebf6).into(),
            term_bg: rgb(0x111110).into(),
            term_header: rgb(0x191817).into(),
            term_fg: rgb(0xedeae2).into(),
            term_muted: rgb(0x9b988e).into(),
            term_border: rgba(0xfaf9f524).into(),
        }
    }

    /// Warm Charcoal dark mode (`#141413` warm ink + `#d97757` terracotta).
    #[must_use]
    pub fn dark() -> Self {
        Self {
            bg: rgb(0x141413).into(),
            surface: rgb(0x1c1b19).into(),
            surface_alt: rgb(0x252421).into(),
            code_bg: rgb(0x191817).into(),
            text: rgb(0xfaf9f5).into(),
            text_2: rgb(0xdedcd3).into(),
            muted: rgb(0xa3a198).into(),
            faint: rgb(0x7a786f).into(),
            border: rgba(0xfaf9f51a).into(),
            border_strong: rgba(0xfaf9f52e).into(),
            primary: rgb(0xd97757).into(),
            primary_hover: rgb(0xe58b6c).into(),
            primary_soft: rgba(0xd9775726).into(),
            on_primary: rgb(0x141413).into(),
            ok: rgb(0x8faf7f).into(),
            ok_soft: rgba(0x8faf7f24).into(),
            warn: rgb(0xd4a853).into(),
            warn_soft: rgba(0xd4a85324).into(),
            err: rgb(0xe07a6e).into(),
            err_soft: rgba(0xe07a6e24).into(),
            info: rgb(0x82a6d8).into(),
            info_soft: rgba(0x82a6d824).into(),
            term_bg: rgb(0x0e0e0d).into(),
            term_header: rgb(0x161614).into(),
            term_fg: rgb(0xfaf9f5).into(),
            term_muted: rgb(0x9b988e).into(),
            term_border: rgba(0xfaf9f524).into(),
        }
    }
}

/// Applies the StaticFlow Warm Editorial palette to `gpui-component`'s global [`Theme`],
/// reconciling tokens and base scrollbar/input projections.
pub fn apply_staticflow_theme(dark: bool, cx: &mut App) {
    let p = Palette::for_mode(dark);
    let mode = if dark {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };
    Theme::change(mode, None, cx);
    Theme::update(cx, |theme| {
        theme.radius = px(8.0);
        theme.shadow = false;
        let c = &mut theme.colors;
        c.background = p.bg;
        c.foreground = p.text;
        c.muted = p.surface_alt;
        c.muted_foreground = p.muted;
        c.border = p.border_strong;
        c.input = p.border_strong;
        c.ring = p.primary;
        c.primary = p.primary;
        c.primary_hover = p.primary_hover;
        c.primary_active = p.primary_hover;
        c.primary_foreground = p.on_primary;
        c.secondary = p.surface_alt;
        c.secondary_hover = p.code_bg;
        c.secondary_active = p.border;
        c.secondary_foreground = p.text;
        c.accent = p.primary_soft;
        c.accent_foreground = p.primary;
        c.popover = p.surface;
        c.popover_foreground = p.text;
        c.sidebar = p.surface;
        c.sidebar_foreground = p.text_2;
        c.sidebar_border = p.border;
        c.sidebar_accent = p.primary_soft;
        c.sidebar_accent_foreground = p.primary;
        c.sidebar_primary = p.primary;
        c.sidebar_primary_foreground = p.on_primary;
        c.success = p.ok;
        c.success_hover = p.ok;
        c.success_active = p.ok;
        c.success_foreground = rgb(0xffffff).into();
        c.warning = p.warn;
        c.warning_hover = p.warn;
        c.warning_active = p.warn;
        c.warning_foreground = rgb(0xffffff).into();
        c.danger = p.err;
        c.danger_hover = p.err;
        c.danger_active = p.err;
        c.danger_foreground = rgb(0xffffff).into();
        c.info = p.info;
        c.info_hover = p.info;
        c.info_active = p.info;
        c.info_foreground = rgb(0xffffff).into();
        c.selection = p.primary_soft;
        c.switch = p.border_strong;
        c.switch_thumb = rgb(0xffffff).into();
        c.scrollbar_thumb = p.border_strong;
        c.scrollbar_thumb_hover = p.muted;
        c.tab_bar = p.surface_alt;
        c.tab_active = p.surface;
        c.tab_active_foreground = p.text;
        c.tab_foreground = p.muted;
        c.list = p.surface;
        c.list_hover = p.surface_alt;
        c.list_active = p.primary_soft;
        c.list_active_border = p.primary;
    });
}

/// Semantic tone for StaticFlow status pills and callouts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadgeTone {
    Primary,
    Brand,
    Ok,
    Warn,
    Err,
    Info,
    Neutral,
}

impl BadgeTone {
    #[must_use]
    pub const fn from_report(tone: ReportTone) -> Self {
        match tone {
            ReportTone::Ok => Self::Ok,
            ReportTone::Info => Self::Info,
            ReportTone::Action => Self::Primary,
            ReportTone::Warn => Self::Warn,
            ReportTone::Err => Self::Err,
            ReportTone::Muted => Self::Neutral,
        }
    }

    #[must_use]
    pub fn colors(self, p: &Palette) -> (Hsla, Hsla, Hsla) {
        match self {
            Self::Primary | Self::Brand => (p.primary_soft, p.primary, p.primary),
            Self::Ok => (p.ok_soft, p.ok, p.ok),
            Self::Warn => (p.warn_soft, p.warn, p.warn),
            Self::Err => (p.err_soft, p.err, p.err),
            Self::Info => (p.info_soft, p.info, p.info),
            Self::Neutral => (p.surface_alt, p.muted, p.border_strong),
        }
    }
}

/// Crisp 1px Warm Editorial surface card (`10px` radius, no heavy drop shadow).
#[must_use]
pub fn editorial_card(p: &Palette) -> Div {
    div()
        .bg(p.surface)
        .border_1()
        .border_color(p.border)
        .rounded(px(10.0))
        .p_4()
        .flex()
        .flex_col()
        .gap_3()
}

/// `9999px` rounded StaticFlow status pill badge with dot indicator.
#[must_use]
pub fn pill_badge(p: &Palette, tone: BadgeTone, label: impl Into<SharedString>) -> Div {
    let (bg_col, fg_col, border_col) = tone.colors(p);
    div()
        .px_2p5()
        .py_0p5()
        .rounded(px(999.0))
        .bg(bg_col)
        .border_1()
        .border_color(border_col.opacity(0.35))
        .flex()
        .items_center()
        .gap_1p5()
        .child(div().size(px(6.0)).rounded(px(999.0)).bg(fg_col))
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(fg_col)
                .child(label.into()),
        )
}

/// Editorial KPI summary tile.
#[must_use]
pub fn kpi_card(
    p: &Palette,
    eyebrow: impl Into<SharedString>,
    value: impl Into<SharedString>,
    subtext: impl Into<SharedString>,
) -> Div {
    div()
        .flex_1()
        .min_w(px(170.0))
        .bg(p.surface)
        .border_1()
        .border_color(p.border)
        .rounded(px(10.0))
        .p_3p5()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(p.muted)
                .child(eyebrow.into()),
        )
        .child(
            div()
                .text_xl()
                .font_weight(FontWeight::BOLD)
                .text_color(p.text)
                .child(value.into()),
        )
        .child(div().text_xs().text_color(p.faint).child(subtext.into()))
}

/// Editorial card header with uppercase index eyebrow, title, and description.
#[must_use]
pub fn section_header(
    p: &Palette,
    eyebrow: &str,
    title: impl Into<SharedString>,
    subtitle: Option<&str>,
) -> Div {
    let mut col = div().flex().flex_col().gap_0p5();
    if !eyebrow.is_empty() {
        col = col.child(
            div()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.primary)
                .child(eyebrow.to_owned()),
        );
    }
    col = col.child(
        div()
            .text_base()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(p.text)
            .child(title.into()),
    );
    if let Some(sub) = subtitle {
        col = col.child(div().text_xs().text_color(p.muted).child(sub.to_owned()));
    }
    col
}

/// Two-column key/value metadata row.
#[must_use]
pub fn kv_row(p: &Palette, key: impl Into<SharedString>, value: impl Into<SharedString>) -> Div {
    div()
        .w_full()
        .py_1()
        .border_b_1()
        .border_color(p.border.opacity(0.6))
        .flex()
        .items_start()
        .justify_between()
        .gap_4()
        .child(
            div()
                .flex_shrink_0()
                .text_xs()
                .text_color(p.muted)
                .child(key.into()),
        )
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(p.text)
                .text_right()
                .child(value.into()),
        )
}

/// Signature `.sf-term` dark editorial terminal output container (`#111110` surface with
/// warm terracotta/ochre/sage window dots and scrollable monospace viewport).
#[must_use]
pub fn sf_terminal_box(
    p: &Palette,
    scroll_id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    content: &str,
    max_height: Pixels,
) -> Div {
    let lines: Vec<String> = if content.trim().is_empty() {
        Vec::new()
    } else {
        content.lines().map(str::to_owned).collect()
    };
    let line_count = lines.len();

    let body = div()
        .id(scroll_id.into())
        .max_h(max_height)
        .min_h(px(96.0))
        .p_3()
        .flex()
        .flex_col()
        .gap_0p5()
        .when(lines.is_empty(), |el| {
            el.child(div().text_xs().text_color(p.term_muted).child("(empty)"))
        })
        .children(lines.into_iter().map(|line| {
            let color = if line.contains("ERROR") || line.contains("error:") {
                rgb(0xe07a6e).into()
            } else if line.contains("WARN") || line.contains("warning:") {
                rgb(0xd4a853).into()
            } else {
                p.term_fg
            };
            div().text_xs().text_color(color).child(if line.is_empty() {
                " ".to_owned()
            } else {
                line
            })
        }))
        .overflow_y_scrollbar();

    div()
        .w_full()
        .bg(p.term_bg)
        .border_1()
        .border_color(p.term_border)
        .rounded(px(10.0))
        .overflow_hidden()
        .flex()
        .flex_col()
        .child(
            div()
                .px_3()
                .py_2()
                .bg(p.term_header)
                .border_b_1()
                .border_color(p.term_border)
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .child(div().size(px(8.0)).rounded(px(999.0)).bg(rgb(0xd97757)))
                        .child(div().size(px(8.0)).rounded(px(999.0)).bg(rgb(0xd4a853)))
                        .child(div().size(px(8.0)).rounded(px(999.0)).bg(rgb(0x8faf7f)))
                        .child(
                            div()
                                .ml_2()
                                .text_xs()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(p.term_muted)
                                .child(title.into()),
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(p.term_muted)
                        .child(format!("{line_count} lines")),
                ),
        )
        .child(body)
}
