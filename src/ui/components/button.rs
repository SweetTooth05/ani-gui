use gpui::{
    App, ClickEvent, ElementId, FontWeight, Hsla, IntoElement, Rgba, SharedString, Window, div,
    prelude::*, px,
};

use crate::ui::theme;

type OnClick = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    /// The one action that plays something: accent fill.
    Primary,
    /// Raised neutral fill.
    Secondary,
    /// Text only until hovered.
    Ghost,
}

#[derive(IntoElement)]
pub struct Button {
    id: ElementId,
    label: SharedString,
    kind: ButtonKind,
    disabled: bool,
    compact: bool,
    on_click: Option<OnClick>,
}

pub fn button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    Button {
        id: id.into(),
        label: label.into(),
        kind: ButtonKind::Primary,
        disabled: false,
        compact: false,
        on_click: None,
    }
}

impl Button {
    pub fn kind(mut self, kind: ButtonKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn compact(mut self) -> Self {
        self.compact = true;
        self
    }

    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Box::new(f));
        self
    }
}

impl RenderOnce for Button {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let light = theme::is_light();
        let (bg, hover_bg, fg, hover_fg): (Hsla, Hsla, Rgba, Rgba) = match self.kind {
            ButtonKind::Primary => (
                theme::accent().into(),
                theme::accent_hover().into(),
                theme::on_accent(),
                theme::on_accent(),
            ),
            ButtonKind::Secondary => (
                theme::control().into(),
                theme::control_hover().into(),
                theme::text(),
                theme::text(),
            ),
            ButtonKind::Ghost => (
                gpui::transparent_black(),
                theme::ghost_hover().into(),
                theme::text_dim(),
                theme::text(),
            ),
        };
        let (h, px_x, size) = if self.compact {
            (px(32.), px(14.), px(theme::SMALL))
        } else {
            (px(44.), px(22.), px(theme::BODY))
        };
        let base = div()
            .id(self.id)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .h(h)
            .px(px_x)
            .rounded(px(8.))
            .bg(bg)
            .when(light && self.kind == ButtonKind::Secondary, |d| {
                d.border_1()
                    .border_color(theme::control_border())
                    .shadow(theme::shadow_sm())
            })
            .when(light && self.kind == ButtonKind::Primary, |d| {
                d.shadow(theme::shadow_sm())
            })
            .text_color(fg)
            .text_size(size)
            .font_weight(FontWeight::SEMIBOLD)
            .whitespace_nowrap()
            .child(self.label);
        if self.disabled {
            return base.opacity(0.45);
        }
        let base = base
            .cursor_pointer()
            .hover(move |s| s.bg(hover_bg).text_color(hover_fg));
        match self.on_click {
            Some(f) => base.on_click(f),
            None => base,
        }
    }
}

/// Mutually exclusive options (Sub/Dub, quality): `options` are
/// (label, selected, on_click).
#[derive(IntoElement)]
pub struct Segmented {
    id: ElementId,
    options: Vec<(SharedString, bool, OnClick)>,
}

pub fn segmented(id: impl Into<ElementId>) -> Segmented {
    Segmented {
        id: id.into(),
        options: Vec::new(),
    }
}

impl Segmented {
    pub fn option(
        mut self,
        label: impl Into<SharedString>,
        selected: bool,
        f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.options.push((label.into(), selected, Box::new(f)));
        self
    }
}

impl RenderOnce for Segmented {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let light = theme::is_light();
        div()
            .id(self.id)
            .flex()
            .flex_none()
            .h(px(44.))
            .p(px(4.))
            .gap(px(2.))
            .rounded(px(10.))
            .bg(theme::track())
            .children(
                self.options
                    .into_iter()
                    .enumerate()
                    .map(|(i, (label, selected, f))| {
                        let seg = div()
                            .id(i)
                            .flex()
                            .items_center()
                            .h_full()
                            .px(px(14.))
                            .rounded(px(7.))
                            .text_size(px(theme::SMALL))
                            .font_weight(FontWeight::SEMIBOLD)
                            .whitespace_nowrap()
                            .child(label);
                        if selected {
                            seg.bg(if light {
                                theme::control()
                            } else {
                                theme::raised_hover()
                            })
                            .text_color(theme::text())
                            .when(light, |s| s.shadow(theme::shadow_sm()))
                        } else {
                            seg.text_color(if light {
                                theme::text_dim()
                            } else {
                                theme::text_muted()
                            })
                            .cursor_pointer()
                            .hover(|s| s.text_color(theme::text()))
                            .on_click(f)
                        }
                    }),
            )
    }
}
