//! Building blocks of the video player's controls (the HUD). They always sit
//! on a picture, never on an app surface, so they use fixed light-on-dark
//! tones; the accent marks only what plays (the play disc, progress).

use gpui::{
    AnyView, App, ClickEvent, Div, ElementId, FontWeight, Hsla, IntoElement, Pixels, SharedString,
    Window, div, prelude::*, px, relative,
};

use crate::ui::components::icon::icon;
use crate::ui::theme;

type OnClick = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// Text and icons on the HUD.
pub fn paper() -> Hsla {
    gpui::white().opacity(0.96)
}
/// Secondary HUD text.
pub fn ash() -> Hsla {
    gpui::white().opacity(0.66)
}
/// An unavailable control.
fn faded() -> Hsla {
    gpui::white().opacity(0.34)
}
/// Translucent disc behind transport buttons, legible on any frame.
fn glass() -> Hsla {
    gpui::black().opacity(0.42)
}
fn glass_hover() -> Hsla {
    gpui::white().opacity(0.16)
}
pub fn hairline() -> Hsla {
    gpui::white().opacity(0.14)
}
fn hover() -> Hsla {
    gpui::white().opacity(0.12)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HudButtonKind {
    /// A bare round icon button for the utility cluster (volume, subtitles…).
    Icon,
    /// A glass disc for the transport and the top bar.
    Glass,
    /// The accent disc that plays and pauses.
    Play,
}

/// A round icon button with a hover state and a tooltip naming it.
#[derive(IntoElement)]
pub struct HudButton {
    id: ElementId,
    glyph: Glyph,
    label: SharedString,
    kind: HudButtonKind,
    disabled: bool,
    active: bool,
    on_click: Option<OnClick>,
}

enum Glyph {
    Icon(&'static str),
    /// A curved arrow with the skip interval inside it.
    Skip {
        forward: bool,
        secs: u32,
    },
}

/// A HUD button showing the Lucide icon `name`, labelled `label` (its
/// tooltip).
pub fn hud_button(
    id: impl Into<ElementId>,
    name: &'static str,
    label: impl Into<SharedString>,
) -> HudButton {
    HudButton {
        id: id.into(),
        glyph: Glyph::Icon(name),
        label: label.into(),
        kind: HudButtonKind::Icon,
        disabled: false,
        active: false,
        on_click: None,
    }
}

/// A glass transport button that skips `secs` seconds back or forward.
pub fn skip_button(
    id: impl Into<ElementId>,
    forward: bool,
    secs: u32,
    label: impl Into<SharedString>,
) -> HudButton {
    HudButton {
        glyph: Glyph::Skip { forward, secs },
        kind: HudButtonKind::Glass,
        ..hud_button(id, "rotate-cw", label)
    }
}

impl HudButton {
    pub fn kind(mut self, kind: HudButtonKind) -> Self {
        self.kind = kind;
        self
    }

    /// Unavailable: faded, no hover, ignores clicks (the tooltip stays).
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Keeps the hover fill, for a button whose menu is open.
    pub fn active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Box::new(f));
        self
    }
}

impl RenderOnce for HudButton {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let (size, glyph_size): (Pixels, Pixels) = match self.kind {
            HudButtonKind::Icon => (px(44.), px(22.)),
            HudButtonKind::Glass => (px(48.), px(24.)),
            HudButtonKind::Play => (px(60.), px(26.)),
        };
        let color: Hsla = match (self.kind, self.disabled) {
            (_, true) => faded(),
            (HudButtonKind::Play, false) => theme::on_accent().into(),
            _ => paper(),
        };
        let glyph = match self.glyph {
            Glyph::Icon(name) => icon(name)
                .size(glyph_size)
                .text_color(color)
                // Lucide's play triangle sits left of centre in its box.
                .when(name == "play", |i| i.ml(px(3.)))
                .into_any_element(),
            Glyph::Skip { forward, secs } => div()
                .relative()
                .size(px(30.))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    icon(if forward { "rotate-cw" } else { "rotate-ccw" })
                        .absolute()
                        .inset_0()
                        .size(px(30.))
                        .text_color(color),
                )
                .child(
                    div()
                        .pt(px(1.))
                        .text_size(px(10.))
                        .font_weight(FontWeight::BOLD)
                        .text_color(color)
                        .child(secs.to_string()),
                )
                .into_any_element(),
        };
        let label = self.label;
        let mut el = div()
            .id(self.id)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(size)
            .rounded_full()
            .child(glyph)
            .tooltip(move |_, cx| tooltip(label.clone(), cx));
        el = match self.kind {
            HudButtonKind::Icon => el.when(self.active, |d| d.bg(hover())),
            HudButtonKind::Glass => el.bg(glass()).border_1().border_color(hairline()),
            HudButtonKind::Play => el.bg(theme::accent()).shadow_lg(),
        };
        if self.disabled {
            return el;
        }
        el = match self.kind {
            HudButtonKind::Icon => el.hover(|s| s.bg(hover())),
            HudButtonKind::Glass => el.hover(|s| s.bg(glass_hover())),
            HudButtonKind::Play => el.hover(|s| s.bg(theme::accent_hover())),
        };
        el.cursor_pointer()
            .when_some(self.on_click, |el, f| el.on_click(f))
    }
}

/// A floating action pill for contextual prompts ("Skip intro", "Next episode")
/// that may sit directly over bright video while the rest of the HUD is hidden.
/// `primary` uses the amber accent (for starting the next episode); otherwise
/// uses a darker frosted glass surface for reliable legibility.
pub fn floating_pill(
    id: impl Into<ElementId>,
    icon_name: &'static str,
    label: impl Into<SharedString>,
    primary: bool,
) -> gpui::Stateful<Div> {
    let fg: Hsla = if primary {
        theme::on_accent().into()
    } else {
        paper()
    };
    let el = div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.))
        .h(px(42.))
        .px(px(18.))
        .rounded_full()
        .text_size(px(14.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(fg)
        .shadow_lg()
        .cursor_pointer()
        .child(icon(icon_name).size(px(16.)).text_color(fg))
        .child(label.into());
    if primary {
        el.bg(theme::accent())
            .hover(|s| s.bg(theme::accent_hover()))
    } else {
        el.bg(gpui::black().opacity(0.68))
            .border_1()
            .border_color(gpui::white().opacity(0.24))
            .hover(|s| s.bg(gpui::black().opacity(0.84)))
    }
}

/// A short vertical rule between groups of controls.
pub fn divider() -> Div {
    div().w(px(1.)).h(px(24.)).mx(px(6.)).bg(hairline())
}

/// The seek bar's track: the played part in the accent, which marks what
/// plays.
pub fn seek_track(fraction: f32, group: &'static str, active: bool) -> Div {
    track(
        fraction,
        group,
        active,
        (px(4.), px(7.)),
        theme::accent().into(),
    )
}

/// The volume slider's track, filled in the HUD's text colour.
pub fn volume_track(fraction: f32, group: &'static str, active: bool) -> Div {
    track(fraction, group, active, (px(4.), px(6.)), paper())
}

/// A thin track filled to `fraction` in `fill`. It grows from its resting
/// to its grown height and shows a handle while its `group` is hovered, and
/// stays so while `active` (being dragged).
fn track(
    fraction: f32,
    group: &'static str,
    active: bool,
    (rest, grown): (Pixels, Pixels),
    fill: Hsla,
) -> Div {
    let fraction = fraction.clamp(0.0, 1.0);
    let handle = div()
        .absolute()
        .top_1_2()
        .left(relative(fraction))
        .mt(px(-7.))
        .ml(px(-7.))
        .size(px(14.))
        .rounded_full()
        .bg(paper())
        .border_2()
        .border_color(gpui::black().opacity(0.28))
        .shadow_md();
    div()
        .relative()
        .w_full()
        .h(if active { grown } else { rest })
        .group_hover(group, |s| s.h(grown))
        .rounded_full()
        .bg(gpui::white().opacity(0.26))
        .child(div().h_full().w(relative(fraction)).rounded_full().bg(fill))
        .child(if active {
            handle
        } else {
            handle.invisible().group_hover(group, |s| s.visible())
        })
}

/// The tooltip shown over a HUD control.
pub fn tooltip(text: SharedString, cx: &mut App) -> AnyView {
    cx.new(|_| HudTooltip(text)).into()
}

struct HudTooltip(SharedString);

impl Render for HudTooltip {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
            .px(px(10.))
            .py(px(5.))
            .rounded(px(8.))
            .bg(theme::art_scrim(0.96))
            .border_1()
            .border_color(hairline())
            .text_size(px(theme::SMALL))
            .font_weight(FontWeight::MEDIUM)
            .text_color(paper())
            .shadow_lg()
            .child(self.0.clone())
    }
}

/// The icon for `volume` (0–100): crossed out when silent, one wave when
/// quiet, two from half volume.
pub fn volume_icon(volume: f64) -> &'static str {
    if volume <= 0.0 {
        "volume-x"
    } else if volume < 50.0 {
        "volume-1"
    } else {
        "volume-2"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_icon_reflects_level() {
        assert_eq!(volume_icon(0.0), "volume-x");
        assert_eq!(volume_icon(-1.0), "volume-x");
        assert_eq!(volume_icon(1.0), "volume-1");
        assert_eq!(volume_icon(49.9), "volume-1");
        assert_eq!(volume_icon(50.0), "volume-2");
        assert_eq!(volume_icon(100.0), "volume-2");
    }
}
