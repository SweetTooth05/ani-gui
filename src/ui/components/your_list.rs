//! The Show page's "Your list" section: the show's status and progress on
//! the user's MyAnimeList/AniList lists, and a 1–10 score picker once it's
//! completed (highlighted as a prompt right after finishing it). Hidden
//! when no account is connected.

use gpui::{
    AnyElement, Context, FontWeight, IntoElement, Subscription, Task, Window, div, prelude::*, px,
    relative,
};

use crate::app::AppState;
use crate::lists::{self, ListEntry, ListStatus, Service};
use crate::ui::accounts;
use crate::ui::components::button::{ButtonKind, button};
use crate::ui::components::icon::icon;
use crate::ui::components::poster::ui_progress_bar;
use crate::ui::components::toast;
use crate::ui::theme;

struct Loaded {
    connected: Vec<Service>,
    entry: Option<ListEntry>,
    ask_rating: bool,
}

pub struct YourList {
    anilist_id: Option<i64>,
    loaded: Option<Loaded>,
    /// The star under the pointer, for the picker's preview.
    hovered: Option<u8>,
    /// A score being sent, shown straight away.
    saving: Option<u8>,
    load_task: Option<Task<()>>,
    _changed: Option<Subscription>,
}

impl YourList {
    pub fn new(cx: &mut Context<Self>) -> Self {
        YourList {
            anilist_id: None,
            loaded: None,
            hovered: None,
            saving: None,
            load_task: None,
            _changed: accounts::observe(cx, |list: &mut Self, cx| list.reload(cx)),
        }
    }

    /// Shows the list entry of `anilist_id` (None: the show isn't on
    /// AniList, so nothing is shown).
    pub fn set_show(&mut self, anilist_id: Option<i64>, cx: &mut Context<Self>) {
        if self.anilist_id != anilist_id {
            self.loaded = None;
        }
        self.anilist_id = anilist_id;
        self.reload(cx);
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let (Some(id), Some(db)) = (
            self.anilist_id,
            cx.try_global::<AppState>().map(|s| s.db.clone()),
        ) else {
            return;
        };
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    Loaded {
                        connected: lists::accounts(&db)
                            .into_iter()
                            .filter(|a| a.connected())
                            .map(|a| a.service)
                            .collect(),
                        entry: lists::entry_for(&db, id),
                        ask_rating: lists::wants_rating(&db, id),
                    }
                })
                .await;
            this.update(cx, |list, cx| {
                if list.anilist_id == Some(id) {
                    list.loaded = Some(loaded);
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn rate(&mut self, score: u8, cx: &mut Context<Self>) {
        let Some(id) = self.anilist_id else {
            return;
        };
        self.saving = Some(score);
        cx.notify();
        let sent = cx.background_spawn(async move { lists::rate(id, f32::from(score)) });
        cx.spawn(async move |this, cx| {
            let result = sent.await;
            this.update(cx, |list, cx| {
                list.saving = None;
                match result {
                    Ok(()) => toast::info(cx, format!("Rated {score}/10")),
                    Err(err) => toast::error(cx, format!("Couldn't send your rating: {err:#}")),
                }
                list.reload(cx);
            })
            .ok();
        })
        .detach();
    }

    fn not_now(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.anilist_id else {
            return;
        };
        if let Some(loaded) = self.loaded.as_mut() {
            loaded.ask_rating = false;
        }
        cx.notify();
        cx.background_spawn(async move {
            if let Err(err) = lists::dismiss_rating(id) {
                eprintln!("ani-gui: dismissing rating prompt: {err:#}");
            }
        })
        .detach();
    }

    fn stars(&self, current: Option<f32>, cx: &mut Context<Self>) -> AnyElement {
        let shown = self
            .hovered
            .or(self.saving)
            .or(current.map(|s| s.round().clamp(0.0, 10.0) as u8))
            .unwrap_or(0);
        let label = match (self.saving, current) {
            (Some(s), _) => format!("{s}/10"),
            (None, Some(s)) if s.fract() == 0.0 => format!("{s:.0}/10"),
            (None, Some(s)) => format!("{s:.1}/10"),
            (None, None) => "Not rated".into(),
        };
        div()
            .flex()
            .items_center()
            .gap(px(14.))
            .child(
                div()
                    .id("your-list-stars")
                    .flex()
                    .items_center()
                    .gap(px(2.))
                    .on_hover(cx.listener(|list, hovered: &bool, _, cx| {
                        if !hovered {
                            list.hovered = None;
                            cx.notify();
                        }
                    }))
                    .children((1..=10u8).map(|n| {
                        let lit = n <= shown;
                        div()
                            .id(("your-list-star", n as usize))
                            .p(px(3.))
                            .rounded(px(6.))
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::wash()))
                            .on_hover(cx.listener(move |list, hovered: &bool, _, cx| {
                                if *hovered && list.hovered != Some(n) {
                                    list.hovered = Some(n);
                                    cx.notify();
                                }
                            }))
                            .on_click(cx.listener(move |list, _, _, cx| list.rate(n, cx)))
                            .child(icon("star").size(px(22.)).text_color(if lit {
                                theme::accent()
                            } else {
                                theme::text_muted()
                            }))
                    })),
            )
            .child(
                div()
                    .min_w(px(64.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(if self.hovered.is_some() || current.is_some() {
                        theme::text()
                    } else {
                        theme::text_muted()
                    })
                    .child(match self.hovered {
                        Some(n) => format!("{n}/10"),
                        None => label,
                    }),
            )
            .into_any_element()
    }
}

/// "Episode 5 of 28", "28 episodes", "Not started".
pub fn progress_label(status: ListStatus, progress: u32, episodes: Option<u32>) -> String {
    match (status, progress, episodes) {
        (ListStatus::Completed, _, Some(total)) if progress >= total => match total {
            1 => "1 episode".into(),
            n => format!("All {n} episodes"),
        },
        (_, 0, _) => "Not started".into(),
        (_, p, Some(total)) => format!("Episode {p} of {total}"),
        (_, p, None) => format!("Episode {p}"),
    }
}

/// Which services the entry is synced with, for the section's subtitle.
fn services_label(connected: &[Service]) -> String {
    let names: Vec<&str> = connected.iter().map(|s| s.label()).collect();
    format!("On {}", names.join(" and "))
}

impl Render for YourList {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(loaded) = &self.loaded else {
            return div().into_any_element();
        };
        if loaded.connected.is_empty() {
            return div().into_any_element();
        }
        let heading = div()
            .flex()
            .items_baseline()
            .gap(px(12.))
            .child(
                div()
                    .font_family(theme::DISPLAY_FONT)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(px(theme::SECTION))
                    .child("Your list"),
            )
            .child(
                div()
                    .text_size(px(theme::SMALL))
                    .text_color(theme::text_muted())
                    .child(services_label(&loaded.connected)),
            );

        let body = match &loaded.entry {
            None => div()
                .text_color(theme::text_dim())
                .child("Not on your list yet. It's added when you finish an episode.")
                .into_any_element(),
            Some(entry) => {
                let fraction = match entry.episodes {
                    Some(total) if total > 0 => entry.progress as f32 / total as f32,
                    _ => 0.0,
                };
                let status = div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .w(px(280.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(
                                div()
                                    .px(px(10.))
                                    .py(px(4.))
                                    .rounded_full()
                                    .bg(theme::control())
                                    .when(theme::is_light(), |d| {
                                        d.border_1()
                                            .border_color(theme::control_border())
                                            .shadow(theme::shadow_sm())
                                    })
                                    .text_size(px(theme::SMALL))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(entry.status.label()),
                            )
                            .child(
                                div()
                                    .text_size(px(theme::SMALL))
                                    .text_color(theme::text_dim())
                                    .child(progress_label(
                                        entry.status,
                                        entry.progress,
                                        entry.episodes,
                                    )),
                            ),
                    )
                    .when(entry.episodes.is_some(), |d| {
                        d.child(ui_progress_bar(fraction))
                    });
                let completed = entry.status == ListStatus::Completed;
                let rating = completed.then(|| {
                    let picker = self.stars(entry.score, cx);
                    if loaded.ask_rating && entry.score.is_none() {
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(10.))
                            .px(px(18.))
                            .py(px(14.))
                            .rounded(px(12.))
                            .bg(theme::deep())
                            .border_l(px(3.))
                            .border_color(theme::accent())
                            .when(theme::is_light(), |d| d.shadow(theme::shadow_md()))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap(px(16.))
                                    .child(
                                        div()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child("You finished it. How was it?"),
                                    )
                                    .child(
                                        button("your-list-not-now", "Not now")
                                            .kind(ButtonKind::Ghost)
                                            .compact()
                                            .on_click(
                                                cx.listener(|list, _, _, cx| list.not_now(cx)),
                                            ),
                                    ),
                            )
                            .child(picker)
                            .into_any_element()
                    } else {
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .child(
                                div()
                                    .text_size(px(theme::SMALL))
                                    .text_color(theme::text_muted())
                                    .child("Your score"),
                            )
                            .child(picker)
                            .into_any_element()
                    }
                });
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_x(px(48.))
                    .gap_y(px(18.))
                    .child(status)
                    .children(rating)
                    .into_any_element()
            }
        };
        div()
            .flex()
            .flex_col()
            .gap(px(16.))
            .px(theme::gutter())
            .pt(px(40.))
            .line_height(relative(1.4))
            .child(heading)
            .child(body)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_labels() {
        use ListStatus::*;
        assert_eq!(progress_label(Watching, 5, Some(28)), "Episode 5 of 28");
        assert_eq!(progress_label(Watching, 5, None), "Episode 5");
        assert_eq!(progress_label(Planning, 0, Some(12)), "Not started");
        assert_eq!(progress_label(Completed, 28, Some(28)), "All 28 episodes");
        assert_eq!(progress_label(Completed, 1, Some(1)), "1 episode");
        // Completed with fewer episodes than the show has: say how many.
        assert_eq!(progress_label(Completed, 10, Some(12)), "Episode 10 of 12");
    }

    #[test]
    fn services_label_names_each_account() {
        assert_eq!(services_label(&[Service::Mal]), "On MyAnimeList");
        assert_eq!(
            services_label(&[Service::Mal, Service::AniList]),
            "On MyAnimeList and AniList"
        );
    }
}
