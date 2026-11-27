use gpui::{
    App, ClickEvent, ElementId, FontWeight, Global, IntoElement, SharedString, Window, div,
    linear_color_stop, linear_gradient, prelude::*, px, relative,
};

use super::icon::icon;
use super::image::rounded_image;
use crate::ui::metadata::CardMeta;
use crate::ui::theme;

type OnClick = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

const RADIUS: f32 = 10.;

/// The single poster card currently under the pointer. Building `info_overlay`
/// only for this card avoids laying out and text-shaping hidden synopses and
/// genre chips for every card on the page every frame.
#[derive(Default)]
struct HoveredPoster(Option<ElementId>);

impl Global for HoveredPoster {}

/// Clears any remembered poster hover (call on page navigation).
pub fn clear_hovered_poster(cx: &mut App) {
    if cx.has_global::<HoveredPoster>() {
        cx.global_mut::<HoveredPoster>().0 = None;
    }
}

/// Portrait cover with the title beneath. With `meta`, a quiet score badge
/// sits in the corner and hovering raises a scrim with the show's facts.
#[derive(IntoElement)]
pub struct PosterCard {
    id: ElementId,
    title: SharedString,
    subtitle: Option<SharedString>,
    cover_url: String,
    meta: Option<CardMeta>,
    on_click: Option<OnClick>,
}

pub fn poster_card(
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    cover_url: &str,
) -> PosterCard {
    PosterCard {
        id: id.into(),
        title: title.into(),
        subtitle: None,
        cover_url: cover_url.to_string(),
        meta: None,
        on_click: None,
    }
}

impl PosterCard {
    pub fn subtitle(mut self, subtitle: impl Into<SharedString>) -> Self {
        self.subtitle = Some(subtitle.into());
        self
    }

    /// Facts for the hover overlay; an empty `meta` adds nothing.
    pub fn meta(mut self, meta: CardMeta) -> Self {
        self.meta = (!meta.is_empty()).then_some(meta);
        self
    }

    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Box::new(f));
        self
    }
}

impl RenderOnce for PosterCard {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let hovered = cx.try_global::<HoveredPoster>().and_then(|h| h.0.as_ref()) == Some(&self.id);
        // Settings › Poster details turns the hover overlay off.
        let details = cx
            .try_global::<crate::app::AppState>()
            .is_none_or(|s| s.settings.poster_details);
        let has_meta = details && self.meta.is_some();
        let hover_id = has_meta.then(|| self.id.clone());
        let light = theme::is_light();
        let card = div()
            .id(self.id)
            .group("poster")
            .flex()
            .flex_col()
            .flex_none()
            .w(px(theme::POSTER_W))
            .gap(px(10.))
            .cursor_pointer()
            .child(
                div()
                    .relative()
                    .w(px(theme::POSTER_W))
                    .h(px(theme::POSTER_H))
                    .rounded(px(RADIUS))
                    .when(light, |d| d.shadow(theme::shadow_sm()))
                    .child(rounded_image(
                        &self.cover_url,
                        px(RADIUS),
                        theme::night(),
                        None,
                    ))
                    .children(
                        self.meta
                            .as_ref()
                            .filter(|_| hovered && details)
                            .map(info_overlay),
                    )
                    .children(
                        self.meta
                            .as_ref()
                            .and_then(|m| m.score.clone())
                            .map(score_badge),
                    )
                    .child(hover_ring("poster")),
            )
            .child(
                div()
                    .px(px(2.))
                    .text_size(px(14.))
                    .line_height(relative(1.25))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if light {
                        theme::text()
                    } else {
                        theme::text_dim()
                    })
                    .group_hover("poster", move |s| {
                        s.text_color(if light {
                            theme::accent()
                        } else {
                            theme::text()
                        })
                    })
                    .line_clamp(2)
                    .child(self.title),
            )
            .children(self.subtitle.map(|s| {
                div()
                    .px(px(2.))
                    .mt(px(-6.))
                    .text_size(px(12.))
                    .text_color(theme::text_muted())
                    .child(s)
            }));
        let card = match hover_id {
            Some(hover_id) => card.on_hover(move |&entered, window, cx| {
                let state = cx.default_global::<HoveredPoster>();
                let changed = if entered {
                    if state.0.as_ref() != Some(&hover_id) {
                        state.0 = Some(hover_id.clone());
                        true
                    } else {
                        false
                    }
                } else if state.0.as_ref() == Some(&hover_id) {
                    state.0 = None;
                    true
                } else {
                    false
                };
                if changed {
                    window.refresh();
                }
            }),
            None => card,
        };
        match self.on_click {
            Some(f) => card.on_click(f),
            None => card,
        }
    }
}

/// 16:9 Continue Watching card: art, a progress bar along its bottom edge,
/// then show title and episode.
#[derive(IntoElement)]
pub struct EpisodeCard {
    id: ElementId,
    title: SharedString,
    episode_label: SharedString,
    image_url: String,
    fraction: f32,
    on_click: Option<OnClick>,
}

pub fn episode_card(
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    episode_label: impl Into<SharedString>,
    image_url: &str,
    fraction: f32,
) -> EpisodeCard {
    EpisodeCard {
        id: id.into(),
        title: title.into(),
        episode_label: episode_label.into(),
        image_url: image_url.to_string(),
        fraction,
        on_click: None,
    }
}

impl EpisodeCard {
    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Box::new(f));
        self
    }
}

impl RenderOnce for EpisodeCard {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let light = theme::is_light();
        let card = div()
            .id(self.id)
            .group("episode-card")
            .flex()
            .flex_col()
            .flex_none()
            .w(px(theme::CARD_W))
            .gap(px(10.))
            .cursor_pointer()
            .child(
                div()
                    .relative()
                    .w(px(theme::CARD_W))
                    .h(px(theme::CARD_H))
                    .rounded(px(RADIUS))
                    .when(light, |d| d.shadow(theme::shadow_sm()))
                    .child(rounded_image(
                        &self.image_url,
                        px(RADIUS),
                        theme::night(),
                        None,
                    ))
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .rounded(px(RADIUS))
                            .bg(linear_gradient(
                                0.,
                                linear_color_stop(theme::art_scrim(0.85), 0.),
                                linear_color_stop(theme::art_scrim(0.), 0.55),
                            )),
                    )
                    .when(self.fraction > 0.0, |art| {
                        art.child(
                            progress_bar(self.fraction)
                                .absolute()
                                .bottom(px(10.))
                                .left(px(12.))
                                .right(px(12.)),
                        )
                    })
                    .child(hover_ring("episode-card")),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .px(px(2.))
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(if light {
                                theme::text()
                            } else {
                                theme::text_dim()
                            })
                            .group_hover("episode-card", move |s| {
                                s.text_color(if light {
                                    theme::accent()
                                } else {
                                    theme::text()
                                })
                            })
                            .truncate()
                            .child(self.title),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::text_muted())
                            .child(self.episode_label),
                    ),
            );
        match self.on_click {
            Some(f) => card.on_click(f),
            None => card,
        }
    }
}

/// The always-on score in the poster's top-left corner: small and glassy,
/// so it reads over any art without competing with it.
fn score_badge(score: String) -> gpui::Div {
    div()
        .absolute()
        .top(px(8.))
        .left(px(8.))
        .flex()
        .items_center()
        .gap(px(4.))
        .pl(px(6.))
        .pr(px(7.))
        .py(px(3.))
        .rounded(px(6.))
        .bg(theme::art_scrim(0.78))
        .text_size(px(12.))
        .line_height(px(14.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::on_art())
        .child(
            icon("star")
                .size(px(11.))
                .text_color(theme::on_art_accent()),
        )
        .child(score)
}

/// The hover reveal: a scrim rising from the poster's foot, carrying status,
/// facts, genres and the start of the synopsis. Mounted only while hovered.
fn info_overlay(meta: &CardMeta) -> gpui::Div {
    let facts = meta.facts.join(" · ");
    div()
        .absolute()
        .inset_0()
        .rounded(px(RADIUS))
        .overflow_hidden()
        .bg(linear_gradient(
            0.,
            linear_color_stop(theme::art_scrim(0.98), 0.),
            linear_color_stop(theme::art_scrim(0.74), 0.7),
        ))
        .flex()
        .flex_col()
        .justify_end()
        .gap(px(7.))
        .px(px(12.))
        .pt(px(36.))
        .pb(px(12.))
        .text_size(px(12.))
        .line_height(relative(1.3))
        .children(meta.status.clone().map(|status| {
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(if meta.airing {
                    theme::on_art_accent()
                } else {
                    theme::on_art()
                })
                .when(meta.airing, |s| {
                    s.child(div().size(px(6.)).rounded_full().bg(theme::on_art_accent()))
                })
                .child(div().truncate().child(status))
        }))
        .when(!facts.is_empty(), |s| {
            s.child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme::on_art_dim())
                    .truncate()
                    .child(facts),
            )
        })
        .when(!meta.genres.is_empty(), |s| {
            s.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(4.))
                    // One row: chips that don't fit are left out.
                    .max_h(px(19.))
                    .overflow_hidden()
                    .children(meta.genres.iter().map(|g| {
                        div()
                            .px(px(6.))
                            .py(px(2.))
                            .rounded(px(4.))
                            .bg(gpui::white().opacity(0.12))
                            .text_size(px(11.))
                            .text_color(theme::on_art_dim())
                            .child(g.clone())
                    })),
            )
        })
        .children(meta.synopsis.clone().map(|s| {
            div()
                .text_size(px(11.5))
                .line_height(relative(1.35))
                .text_color(theme::on_art_dim())
                .line_clamp(3)
                .child(s)
        }))
}

/// Accent outline over a card's art while `group` is hovered.
fn hover_ring(group: &'static str) -> gpui::Div {
    div()
        .absolute()
        .inset_0()
        .rounded(px(RADIUS))
        .border_2()
        .border_color(gpui::transparent_black())
        .group_hover(group, |s| s.border_color(theme::accent()))
}

/// A thin accent bar filled to `fraction` (0..=1), drawn over dark art.
pub fn progress_bar(fraction: f32) -> gpui::Div {
    div()
        .h(px(3.))
        .rounded(px(2.))
        .bg(gpui::white().opacity(0.22))
        .child(
            div()
                .h_full()
                .w(relative(fraction.clamp(0.0, 1.0)))
                .rounded(px(2.))
                .bg(theme::on_art_accent()),
        )
}

/// A thin accent bar filled to `fraction` (0..=1), drawn on a UI surface.
pub fn ui_progress_bar(fraction: f32) -> gpui::Div {
    let track: gpui::Hsla = if theme::is_light() {
        theme::track().into()
    } else {
        gpui::white().opacity(0.22)
    };
    div().h(px(4.)).rounded(px(2.)).bg(track).child(
        div()
            .h_full()
            .w(relative(fraction.clamp(0.0, 1.0)))
            .rounded(px(2.))
            .bg(theme::accent()),
    )
}
