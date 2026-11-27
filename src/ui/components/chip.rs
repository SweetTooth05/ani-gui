//! Pill-shaped toggles: Search filters and the genres and tags on a show
//! page.

use gpui::{
    Div, ElementId, FontWeight, Hsla, Rgba, SharedString, Stateful, div, prelude::*, px, rgb,
};

use crate::ui::components::icon::icon;
use crate::ui::theme;

/// Green for included filters (dark/light variants with AA+ contrast on `raised`).
pub fn positive() -> Rgba {
    if theme::is_light() {
        rgb(0x116932)
    } else {
        rgb(0x4ade80)
    }
}

/// `positive()` at `alpha`, for tinted rows and pills.
pub fn positive_alpha(alpha: f32) -> Hsla {
    let mut c: Hsla = positive().into();
    c.a = alpha;
    c
}

/// Red for excluded filters, following the current palette's `danger`.
pub fn negative() -> Rgba {
    theme::danger()
}

/// `negative()` at `alpha`, for tinted rows and pills.
pub fn negative_alpha(alpha: f32) -> Hsla {
    let mut c: Hsla = negative().into();
    c.a = alpha;
    c
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChipState {
    Off,
    /// Selected, or active.
    On,
}

/// A chip labelled `label`; add `.on_click(..)` to make it act.
pub fn chip(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    state: ChipState,
) -> Stateful<Div> {
    build(id.into(), None, label.into(), state)
}

/// A chip with the UI icon `icon_name` before its label (which may be
/// empty, for an icon-only chip).
pub fn icon_chip(
    id: impl Into<ElementId>,
    icon_name: &'static str,
    label: impl Into<SharedString>,
    state: ChipState,
) -> Stateful<Div> {
    build(id.into(), Some(icon_name), label.into(), state)
}

/// A compact removable pill for an active genre or tag filter: green `+ label ×`
/// when included, red `− label ×` when excluded. Add `.on_click(..)` to remove.
pub fn filter_pill(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    included: bool,
) -> Stateful<Div> {
    let light = theme::is_light();
    let (mark, fg, bg, hover_bg, border, x_fg) = if included {
        (
            "plus",
            positive(),
            positive_alpha(if light { 0.13 } else { 0.15 }),
            positive_alpha(if light { 0.22 } else { 0.24 }),
            positive_alpha(if light { 0.45 } else { 0.55 }),
            positive_alpha(0.75),
        )
    } else {
        (
            "minus",
            negative(),
            negative_alpha(if light { 0.13 } else { 0.15 }),
            negative_alpha(if light { 0.22 } else { 0.24 }),
            negative_alpha(if light { 0.45 } else { 0.55 }),
            negative_alpha(0.75),
        )
    };
    div()
        .id(id.into())
        .flex()
        .flex_none()
        .items_center()
        .gap(px(5.))
        .h(px(26.))
        .px(px(9.))
        .rounded_full()
        .border_1()
        .bg(bg)
        .border_color(border)
        .text_color(fg)
        .text_size(px(12.))
        .font_weight(FontWeight::MEDIUM)
        .whitespace_nowrap()
        .cursor_pointer()
        .hover(move |s| s.bg(hover_bg).border_color(fg))
        .child(icon(mark).size(px(12.)).text_color(fg))
        .child(label.into())
        .child(icon("x").size(px(12.)).text_color(x_fg))
}

fn build(
    id: ElementId,
    icon_name: Option<&'static str>,
    label: SharedString,
    state: ChipState,
) -> Stateful<Div> {
    let icon_color = match state {
        ChipState::Off => theme::text_muted(),
        ChipState::On => theme::accent(),
    };
    let label = (!label.is_empty()).then_some(label);
    let base = div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.))
        .h(px(30.))
        .px(px(if label.is_some() { 12. } else { 8. }))
        .rounded_full()
        .border_1()
        .text_size(px(theme::SMALL))
        .font_weight(FontWeight::MEDIUM)
        .whitespace_nowrap()
        .cursor_pointer()
        .children(icon_name.map(|name| icon(name).size(px(14.)).text_color(icon_color)))
        .children(label);
    match state {
        ChipState::Off => base
            .bg(theme::control())
            .border_color(if theme::is_light() {
                theme::control_border()
            } else {
                theme::raised()
            })
            .text_color(theme::text_dim())
            .hover(|s| s.bg(theme::control_hover()).text_color(theme::text())),
        ChipState::On => base
            .bg(theme::accent_alpha(0.16))
            .border_color(theme::accent())
            .text_color(theme::text())
            .font_weight(FontWeight::SEMIBOLD),
    }
}
