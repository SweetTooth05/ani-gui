//! The show page's Reviews section: public AniList and MyAnimeList reviews,
//! loaded in the background so the page never waits on them. Review text is
//! plain text from `crate::reviews`; spoilers stay hidden until asked for.

use std::collections::HashSet;

use gpui::{
    AnyElement, Context, FontWeight, IntoElement, Task, Window, div, prelude::*, px, relative,
};

use crate::app::AppState;
use crate::reviews::{self, Review, ReviewSource};
use crate::ui::components::button::{ButtonKind, button, segmented};
use crate::ui::components::icon::icon;
use crate::ui::metadata::thousands;
use crate::ui::theme;
use crate::ui::view_models::clamp_words;

/// Characters of body text a collapsed review shows, in at most
/// `PREVIEW_BLOCKS` paragraphs (some reviews are one-line paragraphs).
const PREVIEW_CHARS: usize = 420;
const PREVIEW_BLOCKS: usize = 4;

enum Load {
    Loading,
    Ready(Vec<Review>),
    Failed(String),
}

pub struct ReviewsSection {
    anilist_id: i64,
    mal_id: Option<i64>,
    source: ReviewSource,
    anilist: Load,
    mal: Load,
    /// Reviews shown in full, by (source, index).
    expanded: HashSet<(ReviewSource, usize)>,
    /// Reviews whose spoilers the user chose to see.
    revealed: HashSet<(ReviewSource, usize)>,
    anilist_task: Option<Task<()>>,
    mal_task: Option<Task<()>>,
}

impl ReviewsSection {
    pub fn new(anilist_id: i64, mal_id: Option<i64>, cx: &mut Context<Self>) -> Self {
        let mut section = ReviewsSection {
            anilist_id,
            mal_id,
            source: ReviewSource::AniList,
            anilist: Load::Loading,
            mal: Load::Loading,
            expanded: HashSet::new(),
            revealed: HashSet::new(),
            anilist_task: None,
            mal_task: None,
        };
        section.load(ReviewSource::AniList, cx);
        if mal_id.is_some() {
            section.load(ReviewSource::MyAnimeList, cx);
        }
        section
    }

    /// The show these reviews are for.
    pub fn anilist_id(&self) -> i64 {
        self.anilist_id
    }

    fn load(&mut self, source: ReviewSource, cx: &mut Context<Self>) {
        let Some(db) = cx.try_global::<AppState>().map(|s| s.db.clone()) else {
            return;
        };
        let (anilist_id, mal_id) = (self.anilist_id, self.mal_id);
        *self.state_mut(source) = Load::Loading;
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    match (source, mal_id) {
                        (ReviewSource::AniList, _) => reviews::anilist(&db, anilist_id),
                        (ReviewSource::MyAnimeList, Some(mal_id)) => {
                            reviews::myanimelist(&db, mal_id)
                        }
                        (ReviewSource::MyAnimeList, None) => Ok(Vec::new()),
                    }
                })
                .await;
            this.update(cx, |section, cx| {
                *section.state_mut(source) = match result {
                    Ok(list) => Load::Ready(list),
                    Err(err) => Load::Failed(friendly_error(&err)),
                };
                cx.notify();
            })
            .ok();
        });
        match source {
            ReviewSource::AniList => self.anilist_task = Some(task),
            ReviewSource::MyAnimeList => self.mal_task = Some(task),
        }
        cx.notify();
    }

    fn state(&self, source: ReviewSource) -> &Load {
        match source {
            ReviewSource::AniList => &self.anilist,
            ReviewSource::MyAnimeList => &self.mal,
        }
    }

    fn state_mut(&mut self, source: ReviewSource) -> &mut Load {
        match source {
            ReviewSource::AniList => &mut self.anilist,
            ReviewSource::MyAnimeList => &mut self.mal,
        }
    }

    fn toggle(set: &mut HashSet<(ReviewSource, usize)>, key: (ReviewSource, usize)) {
        if !set.remove(&key) {
            set.insert(key);
        }
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.mal_id.is_none() {
            return div()
                .text_size(px(theme::SMALL))
                .text_color(theme::text_muted())
                .child("From AniList")
                .into_any_element();
        }
        let mut tabs = segmented("review-source");
        for source in [ReviewSource::AniList, ReviewSource::MyAnimeList] {
            let label = match self.state(source) {
                Load::Ready(list) if !list.is_empty() => {
                    format!("{} {}", source.label(), list.len())
                }
                _ => source.label().to_string(),
            };
            tabs = tabs.option(
                label,
                self.source == source,
                cx.listener(move |section, _, _, cx| {
                    section.source = source;
                    cx.notify();
                }),
            );
        }
        tabs.into_any_element()
    }

    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let source = self.source;
        match self.state(source) {
            Load::Loading => div()
                .flex()
                .items_center()
                .gap(px(10.))
                .text_color(theme::text_muted())
                .child(
                    icon("loader-circle")
                        .size(px(16.))
                        .text_color(theme::text_muted()),
                )
                .child(format!("Loading {} reviews…", source.label()))
                .into_any_element(),
            Load::Failed(err) => div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(12.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .text_color(theme::text_dim())
                        .child(
                            icon("circle-alert")
                                .size(px(16.))
                                .text_color(theme::danger()),
                        )
                        .child(format!("Couldn't load {} reviews: {err}", source.label())),
                )
                .child(
                    button("reviews-retry", "Try again")
                        .kind(ButtonKind::Secondary)
                        .compact()
                        .on_click(cx.listener(move |section, _, _, cx| section.load(source, cx))),
                )
                .into_any_element(),
            Load::Ready(list) if list.is_empty() => div()
                .text_color(theme::text_dim())
                .child(format!(
                    "No one has reviewed this on {} yet.",
                    source.label()
                ))
                .into_any_element(),
            Load::Ready(list) => div()
                .flex()
                .flex_col()
                .max_w(px(780.))
                .children(
                    list.iter()
                        .enumerate()
                        .map(|(i, review)| self.render_review(i, review, cx)),
                )
                .into_any_element(),
        }
    }

    fn render_review(&self, index: usize, review: &Review, cx: &mut Context<Self>) -> AnyElement {
        let source = self.source;
        let key = (source, index);
        let expanded = self.expanded.contains(&key);
        let revealed = self.revealed.contains(&key);
        let all_spoiler = review.body.iter().all(|s| s.spoiler);
        let blocks = blocks(review, revealed);
        let (shown, cut) = if expanded {
            (blocks, false)
        } else {
            preview(blocks, PREVIEW_CHARS)
        };

        let header = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_x(px(14.))
            .gap_y(px(4.))
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text())
                    .child(review.author.clone()),
            )
            .children(review.score.map(|score| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .text_size(px(theme::SMALL))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text())
                    .child(icon("star").size(px(13.)).text_color(theme::accent()))
                    .child(score_text(score))
            }))
            .children(review.verdict.clone().map(|verdict| {
                div()
                    .text_size(px(theme::SMALL))
                    .text_color(theme::text_dim())
                    .child(verdict)
            }))
            .child(div().flex_1())
            .children(review.helpful.map(|helpful| {
                div()
                    .text_size(px(12.))
                    .text_color(theme::text_muted())
                    .child(helpful_label(helpful))
            }));

        let mut actions: Vec<AnyElement> = Vec::new();
        if cut || expanded {
            actions.push(
                button(
                    ("review-more", index),
                    if expanded { "Show less" } else { "Read more" },
                )
                .kind(ButtonKind::Ghost)
                .compact()
                .on_click(cx.listener(move |section, _, _, cx| {
                    Self::toggle(&mut section.expanded, key);
                    cx.notify();
                }))
                .into_any_element(),
            );
        }
        if review.has_spoilers() {
            actions.push(
                button(
                    ("review-spoilers", index),
                    if revealed {
                        "Hide spoilers"
                    } else {
                        "Show spoilers"
                    },
                )
                .kind(ButtonKind::Ghost)
                .compact()
                .on_click(cx.listener(move |section, _, _, cx| {
                    Self::toggle(&mut section.revealed, key);
                    cx.notify();
                }))
                .into_any_element(),
            );
        }
        if let Some(url) = review.url.clone().filter(|_| expanded) {
            let label = if review.shortened {
                format!("Read the rest on {}", source.label())
            } else {
                format!("Open on {}", source.label())
            };
            actions.push(
                div()
                    .id(("review-link", index))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .h(px(32.))
                    .px(px(14.))
                    .rounded(px(8.))
                    .cursor_pointer()
                    .text_size(px(theme::SMALL))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text_dim())
                    .hover(|s| s.bg(theme::ghost_hover()).text_color(theme::text()))
                    .child(label)
                    .child(
                        icon("external-link")
                            .size(px(13.))
                            .text_color(theme::text_dim()),
                    )
                    .on_click(move |_, _, cx| cx.open_url(&url))
                    .into_any_element(),
            );
        }

        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .py(px(24.))
            .when(index > 0, |d| d.border_t_1().border_color(theme::line()))
            .child(header)
            .children(review.summary.clone().map(|summary| {
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight::MEDIUM)
                    .line_height(relative(1.4))
                    .text_color(theme::text())
                    .child(summary)
            }))
            .child(
                div().flex().flex_col().gap(px(10.)).children(
                    shown
                        .into_iter()
                        .enumerate()
                        .map(|(b, block)| render_block(block, all_spoiler, (index, b), key, cx)),
                ),
            )
            .when(!actions.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(4.))
                        .ml(px(-14.))
                        .children(actions),
                )
            })
            .into_any_element()
    }
}

impl Render for ReviewsSection {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .px(theme::gutter())
            .pt(px(8.))
            .pb(px(64.))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap(px(16.))
                    .max_w(px(780.))
                    .pb(px(8.))
                    .child(
                        div()
                            .font_family(theme::DISPLAY_FONT)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(px(theme::SECTION))
                            .child("Reviews"),
                    )
                    .child(self.render_tabs(cx)),
            )
            .child(self.render_body(cx))
    }
}

/// A displayable piece of a review body.
#[derive(Clone, Debug, PartialEq)]
enum Block {
    Para {
        text: String,
        spoiler: bool,
    },
    /// A spoiler the user hasn't asked to see.
    Hidden,
}

/// The review body as paragraphs, spoilers collapsed to one `Hidden` each
/// unless `revealed`.
fn blocks(review: &Review, revealed: bool) -> Vec<Block> {
    let mut out = Vec::new();
    for seg in &review.body {
        if seg.spoiler && !revealed {
            out.push(Block::Hidden);
            continue;
        }
        out.extend(
            seg.text
                .split('\n')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(|p| Block::Para {
                    text: p.to_string(),
                    spoiler: seg.spoiler,
                }),
        );
    }
    out
}

/// The leading blocks worth about `budget` characters, the last paragraph
/// cut at a word; and whether anything was left out.
fn preview(blocks: Vec<Block>, budget: usize) -> (Vec<Block>, bool) {
    let total = blocks.len();
    let mut left = budget;
    let mut out = Vec::new();
    for block in blocks {
        if left == 0 || out.len() == PREVIEW_BLOCKS {
            break;
        }
        match block {
            Block::Para { text, spoiler } if text.chars().count() > left => {
                out.push(Block::Para {
                    text: clamp_words(&text, left),
                    spoiler,
                });
                return (out, true);
            }
            Block::Para { text, spoiler } => {
                left -= text.chars().count();
                out.push(Block::Para { text, spoiler });
            }
            Block::Hidden => out.push(Block::Hidden),
        }
    }
    let cut = out.len() < total;
    (out, cut)
}

fn render_block(
    block: Block,
    all_spoiler: bool,
    id: (usize, usize),
    key: (ReviewSource, usize),
    cx: &mut Context<ReviewsSection>,
) -> AnyElement {
    match block {
        Block::Para { text, spoiler } => div()
            .text_size(px(theme::BODY))
            .line_height(relative(1.6))
            .text_color(theme::text_dim())
            .when(spoiler, |d| {
                d.pl(px(12.))
                    .border_l_2()
                    .border_color(gpui::Hsla::from(theme::accent()).opacity(0.4))
            })
            .child(text)
            .into_any_element(),
        Block::Hidden => div()
            .flex()
            .child(
                div()
                    .id(("review-hidden", id.0 * 10_000 + id.1))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(12.))
                    .py(px(7.))
                    .rounded(px(8.))
                    .bg(theme::control())
                    .when(theme::is_light(), |d| {
                        d.border_1()
                            .border_color(theme::control_border())
                            .shadow(theme::shadow_sm())
                    })
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::control_hover()))
                    .text_size(px(theme::SMALL))
                    .text_color(theme::text_dim())
                    .child(icon("info").size(px(14.)).text_color(theme::text_muted()))
                    .child(if all_spoiler {
                        "This review contains spoilers. Click to show it."
                    } else {
                        "Spoiler hidden. Click to show."
                    })
                    .on_click(cx.listener(move |section, _, _, cx| {
                        section.revealed.insert(key);
                        cx.notify();
                    })),
            )
            .into_any_element(),
    }
}

/// "10" or "8.5".
fn score_text(score: f32) -> String {
    if score.fract() == 0.0 {
        format!("{score:.0}")
    } else {
        format!("{score:.1}")
    }
}

fn helpful_label((up, total): (u32, Option<u32>)) -> String {
    match total {
        Some(total) => format!(
            "{} of {} found this helpful",
            thousands(up),
            thousands(total)
        ),
        None if up == 1 => "1 reaction".into(),
        None => format!("{} reactions", thousands(up)),
    }
}

/// A fetch error in a sentence: timeouts and rate limits in plain words,
/// anything else trimmed to its first line.
fn friendly_error(err: &anyhow::Error) -> String {
    let text = format!("{err:#}");
    if text.contains("429") {
        return "the site is rate limiting requests. Try again in a minute.".into();
    }
    if text.to_lowercase().contains("timed out") || text.to_lowercase().contains("timeout") {
        return "the site didn't answer in time.".into();
    }
    let first = text.lines().next().unwrap_or_default();
    clamp_words(first, 160)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reviews::Segment;

    fn review(body: &[(&str, bool)]) -> Review {
        Review {
            author: "a".into(),
            score: None,
            helpful: None,
            summary: None,
            body: body
                .iter()
                .map(|(text, spoiler)| Segment {
                    text: text.to_string(),
                    spoiler: *spoiler,
                })
                .collect(),
            verdict: None,
            url: None,
            shortened: false,
        }
    }

    fn para(text: &str, spoiler: bool) -> Block {
        Block::Para {
            text: text.into(),
            spoiler,
        }
    }

    #[test]
    fn blocks_hide_spoilers_until_revealed() {
        let r = review(&[("One\n\nTwo", false), ("Dies", true), ("Three", false)]);
        assert_eq!(
            blocks(&r, false),
            [
                para("One", false),
                para("Two", false),
                Block::Hidden,
                para("Three", false)
            ]
        );
        assert_eq!(blocks(&r, true)[2], para("Dies", true));
    }

    #[test]
    fn preview_cuts_at_budget() {
        let all = vec![
            para("aaaa", false),
            Block::Hidden,
            para("bbbb cccc dddd", false),
        ];
        assert_eq!(preview(all.clone(), 100), (all.clone(), false));
        let (shown, cut) = preview(all.clone(), 10);
        assert!(cut);
        assert_eq!(shown[..2], all[..2]);
        assert_eq!(shown[2], para("bbbb…", false));
        let (shown, cut) = preview(all, 4);
        assert!(cut);
        assert_eq!(shown, [para("aaaa", false)]);
        assert_eq!(preview(Vec::new(), 10), (Vec::new(), false));
        let many = vec![para("a", false); PREVIEW_BLOCKS + 1];
        let (shown, cut) = preview(many, 100);
        assert!(cut);
        assert_eq!(shown.len(), PREVIEW_BLOCKS);
    }

    #[test]
    fn labels() {
        assert_eq!(score_text(10.0), "10");
        assert_eq!(score_text(6.5), "6.5");
        assert_eq!(
            helpful_label((336, Some(385))),
            "336 of 385 found this helpful"
        );
        assert_eq!(helpful_label((1204, None)), "1,204 reactions");
        assert_eq!(helpful_label((1, None)), "1 reaction");
        assert_eq!(
            friendly_error(&anyhow::anyhow!("GET x: http status: 429")),
            "the site is rate limiting requests. Try again in a minute."
        );
        assert_eq!(
            friendly_error(&anyhow::anyhow!("io: Timed out")),
            "the site didn't answer in time."
        );
    }
}
