//! My List: the shows the user saved, newest first, then what their
//! MyAnimeList/AniList lists hold, grouped by status.

use std::collections::HashSet;
use std::sync::{Mutex, PoisonError};

use anyhow::Result;
use gpui::{
    AnyElement, Context, FontWeight, IntoElement, ScrollHandle, Subscription, Task, WeakEntity,
    Window, div, prelude::*, px, relative,
};

use crate::app::{AppState, Root, Route, ShowRef};
use crate::db::{self, Db};
use crate::lists::{self, ListEntry, ListStatus};
use crate::model::{Mode, ShowRecord};
use crate::ui::accounts;
use crate::ui::calendar::local_offset;
use crate::ui::components::button::{ButtonKind, button};
use crate::ui::components::poster::poster_card;
use crate::ui::components::your_list::progress_label;
use crate::ui::metadata::CardMeta;
use crate::ui::theme;
use crate::ui::view_models::mode_label;

/// Cards an imported group shows before "Show all"; Watching shows all.
const GROUP_PREVIEW: usize = 10;

struct Loaded {
    saved: Vec<ShowRecord>,
    imported: Vec<ListEntry>,
}

enum State {
    Loading,
    Ready(Loaded),
    Failed(String),
}

pub struct MyListPage {
    root: WeakEntity<Root>,
    state: State,
    /// Imported groups opened past their preview.
    expanded: HashSet<ListStatus>,
    load_task: Option<Task<()>>,
    scroll: ScrollHandle,
    _lists_changed: Option<Subscription>,
}

impl MyListPage {
    pub fn new(root: WeakEntity<Root>, cx: &mut Context<Self>) -> Self {
        let mut page = MyListPage {
            root,
            state: State::Loading,
            expanded: HashSet::new(),
            load_task: None,
            scroll: ScrollHandle::new(),
            _lists_changed: accounts::observe(cx, |page: &mut Self, cx| page.reload(cx)),
        };
        page.reload(cx);
        page
    }

    /// Reads the list again (it changes on Show pages and with syncs).
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(db) = cx.try_global::<AppState>().map(|s| s.db.clone()) else {
            return;
        };
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let loaded = cx.background_spawn(async move { load(&db) }).await;
            this.update(cx, |page, cx| {
                page.load_task = None;
                page.state = match loaded {
                    Ok(loaded) => State::Ready(loaded),
                    Err(err) => State::Failed(format!("{err:#}")),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    fn navigate(&self, route: Route, cx: &mut Context<Self>) {
        self.root
            .update(cx, |root, cx| root.navigate(route, cx))
            .ok();
    }

    fn render_saved(&self, shows: &[ShowRecord], mode: Mode, cx: &mut Context<Self>) -> AnyElement {
        let now = db::now();
        let tz = local_offset(now);
        grid(shows.iter().enumerate().map(|(i, record)| {
            let show = &record.summary;
            let cover = record
                .media
                .as_ref()
                .map(|m| m.cover_url.as_str())
                .filter(|c| !c.is_empty())
                .unwrap_or(&show.cover_url);
            let route = Route::Show(ShowRef {
                allanime: Some(show.clone()),
                anilist_id: show.anilist_id,
                title: show.display_title().to_string(),
            });
            let meta = match &record.media {
                Some(media) => CardMeta::from_media(media, now, tz),
                None => CardMeta::from_summary(show),
            };
            poster_card(("my-list", i), show.display_title().to_string(), cover)
                .meta(meta)
                .subtitle(episodes_label(
                    show.count(mode),
                    show.count(other(mode)),
                    mode,
                ))
                .on_click(cx.listener(move |page, _, _, cx| page.navigate(route.clone(), cx)))
                .into_any_element()
        }))
    }

    /// One status group of imported entries. Cards open the Show page by
    /// AniList id, like the Home and Calendar rows.
    fn render_group(
        &self,
        status: ListStatus,
        entries: &[&ListEntry],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let all = status == ListStatus::Watching || self.expanded.contains(&status);
        let shown = if all {
            entries.len()
        } else {
            entries.len().min(GROUP_PREVIEW)
        };
        let cards = entries[..shown].iter().enumerate().map(|(i, entry)| {
            let route = Route::Show(ShowRef {
                allanime: None,
                anilist_id: Some(entry.anilist_id),
                title: entry.title.clone(),
            });
            let card = poster_card((status.as_str(), i), entry.title.clone(), &entry.cover_url)
                .on_click(cx.listener(move |page, _, _, cx| page.navigate(route.clone(), cx)));
            match entry_subtitle(entry) {
                Some(subtitle) => card.subtitle(subtitle),
                None => card,
            }
            .into_any_element()
        });
        let hidden = entries.len() - shown;
        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(
                group_heading(status.label(), entries.len()).children((hidden > 0).then(|| {
                    button(
                        ("show-all", status as usize),
                        format!("Show all {}", entries.len()),
                    )
                    .kind(ButtonKind::Ghost)
                    .compact()
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.expanded.insert(status);
                        cx.notify();
                    }))
                })),
            )
            .child(grid(cards))
            .into_any_element()
    }

    fn render_body(&self, mode: Mode, cx: &mut Context<Self>) -> AnyElement {
        match &self.state {
            State::Loading => div().into_any_element(),
            State::Failed(err) => div()
                .flex()
                .flex_col()
                .items_start()
                .gap(px(14.))
                .child(
                    div()
                        .max_w(px(640.))
                        .text_color(theme::danger())
                        .child(format!("Couldn't read My List: {err}")),
                )
                .child(
                    button("my-list-retry", "Try again")
                        .kind(ButtonKind::Secondary)
                        .compact()
                        .on_click(cx.listener(|page, _, _, cx| page.reload(cx))),
                )
                .into_any_element(),
            State::Ready(loaded) if loaded.saved.is_empty() && loaded.imported.is_empty() => {
                div()
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap(px(16.))
                    .pt(px(12.))
                    .child(
                        div()
                            .font_family(theme::DISPLAY_FONT)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(px(theme::SECTION))
                            .child("Nothing saved yet"),
                    )
                    .child(
                        div()
                            .max_w(px(520.))
                            .line_height(relative(1.5))
                            .text_color(theme::text_dim())
                            .child(
                                "Press + My List on any show to keep it here, ready for later. \
                             Or connect MyAnimeList or AniList in Settings to bring your list.",
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(10.))
                            .child(
                                button("my-list-search", "Find a show")
                                    .kind(ButtonKind::Secondary)
                                    .on_click(cx.listener(|page, _, _, cx| {
                                        page.navigate(Route::Search, cx)
                                    })),
                            )
                            .child(
                                button("my-list-accounts", "Connect an account")
                                    .kind(ButtonKind::Ghost)
                                    .on_click(cx.listener(|page, _, _, cx| {
                                        page.navigate(Route::Settings, cx)
                                    })),
                            ),
                    )
                    .into_any_element()
            }
            State::Ready(loaded) => {
                let mut body = div().flex().flex_col().gap(px(44.));
                if !loaded.saved.is_empty() {
                    let saved = self.render_saved(&loaded.saved, mode, cx);
                    body = body.child(if loaded.imported.is_empty() {
                        saved
                    } else {
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(18.))
                            .child(group_heading("Saved", loaded.saved.len()))
                            .child(saved)
                            .into_any_element()
                    });
                }
                for (status, entries) in groups(&loaded.imported) {
                    body = body.child(self.render_group(status, &entries, cx));
                }
                body.into_any_element()
            }
        }
    }
}

impl Render for MyListPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = cx
            .try_global::<AppState>()
            .map_or(Mode::default(), |s| s.settings.mode);
        let count = match &self.state {
            State::Ready(l) if !(l.saved.is_empty() && l.imported.is_empty()) => {
                Some(match l.saved.len() + l.imported.len() {
                    1 => "1 show".to_string(),
                    n => format!("{n} shows"),
                })
            }
            _ => None,
        };
        div()
            .id("my-list")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(28.))
                    .px(theme::gutter())
                    .pt(px(48.))
                    .pb(px(64.))
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap(px(16.))
                            .child(
                                div()
                                    .font_family(theme::DISPLAY_FONT)
                                    .font_weight(FontWeight::BOLD)
                                    .text_size(px(theme::PAGE_TITLE))
                                    .line_height(px(theme::PAGE_TITLE))
                                    .child("My List"),
                            )
                            .children(count.map(|c| {
                                div()
                                    .text_size(px(theme::BODY))
                                    .text_color(theme::text_muted())
                                    .child(c)
                            })),
                    )
                    .child(self.render_body(mode, cx)),
            )
    }
}

fn grid(cards: impl Iterator<Item = AnyElement>) -> AnyElement {
    div()
        .flex()
        .flex_wrap()
        .gap_x(px(22.))
        .gap_y(px(30.))
        .children(cards)
        .into_any_element()
}

fn group_heading(title: &'static str, count: usize) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(14.))
        .child(
            div()
                .font_family(theme::DISPLAY_FONT)
                .font_weight(FontWeight::SEMIBOLD)
                .text_size(px(theme::SECTION))
                .child(title),
        )
        .child(
            div()
                .text_size(px(theme::SMALL))
                .text_color(theme::text_muted())
                .child(count.to_string()),
        )
        .child(div().flex_1())
}

/// Imported entries by status, in `ListStatus::ALL` order, empty groups
/// left out; each group keeps the list's newest-first order.
fn groups(entries: &[ListEntry]) -> Vec<(ListStatus, Vec<&ListEntry>)> {
    ListStatus::ALL
        .into_iter()
        .map(|status| {
            let group: Vec<&ListEntry> = entries.iter().filter(|e| e.status == status).collect();
            (status, group)
        })
        .filter(|(_, group)| !group.is_empty())
        .collect()
}

/// A card's second line: the score for rated finished shows, else how far
/// along the user is. Nothing for untouched plans.
fn entry_subtitle(entry: &ListEntry) -> Option<String> {
    match (entry.status, entry.score) {
        (ListStatus::Completed, Some(score)) if score.fract() == 0.0 => {
            Some(format!("Rated {score:.0}/10"))
        }
        (ListStatus::Completed, Some(score)) => Some(format!("Rated {score:.1}/10")),
        (ListStatus::Planning, _) if entry.progress == 0 => entry.episodes.map(|n| match n {
            1 => "1 episode".to_string(),
            n => format!("{n} episodes"),
        }),
        _ => Some(progress_label(entry.status, entry.progress, entry.episodes)),
    }
}

fn other(mode: Mode) -> Mode {
    match mode {
        Mode::Sub => Mode::Dub,
        Mode::Dub => Mode::Sub,
    }
}

/// "24 episodes", or the other mode's count when `mode` has none.
fn episodes_label(count: u32, other_count: u32, mode: Mode) -> String {
    match (count, other_count) {
        (0, 0) => "No episodes yet".into(),
        (0, 1) => format!("1 episode in {}", mode_label(other(mode))),
        (0, n) => format!("{n} episodes in {}", mode_label(other(mode))),
        (1, _) => "1 episode".into(),
        (n, _) => format!("{n} episodes"),
    }
}

/// The saved shows, newest first (ids without a stored show are skipped),
/// and the imported list entries.
fn load(db: &Mutex<Db>) -> Result<Loaded> {
    let saved = {
        let db = db.lock().unwrap_or_else(PoisonError::into_inner);
        let mut shows = Vec::new();
        for id in db.my_list()? {
            if let Some(record) = db.get_show(&id)? {
                shows.push(record);
            }
        }
        shows
    };
    Ok(Loaded {
        saved,
        imported: lists::my_list(db)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: i64, status: ListStatus, progress: u32, score: Option<f32>) -> ListEntry {
        ListEntry {
            anilist_id: id,
            mal_id: None,
            title: format!("Show {id}"),
            cover_url: String::new(),
            status,
            progress,
            score,
            episodes: Some(12),
            updated_at: 100 - id,
        }
    }

    #[test]
    fn groups_follow_status_order_and_skip_empty() {
        let entries = [
            entry(1, ListStatus::Completed, 12, None),
            entry(2, ListStatus::Watching, 3, None),
            entry(3, ListStatus::Completed, 12, None),
            entry(4, ListStatus::Dropped, 1, None),
        ];
        let got: Vec<(ListStatus, Vec<i64>)> = groups(&entries)
            .into_iter()
            .map(|(s, g)| (s, g.iter().map(|e| e.anilist_id).collect()))
            .collect();
        assert_eq!(
            got,
            [
                (ListStatus::Watching, vec![2]),
                (ListStatus::Completed, vec![1, 3]),
                (ListStatus::Dropped, vec![4]),
            ]
        );
        assert!(groups(&[]).is_empty());
    }

    #[test]
    fn subtitles() {
        let sub = |e: ListEntry| entry_subtitle(&e);
        assert_eq!(
            sub(entry(1, ListStatus::Completed, 12, Some(9.0))).as_deref(),
            Some("Rated 9/10")
        );
        assert_eq!(
            sub(entry(1, ListStatus::Completed, 12, Some(8.5))).as_deref(),
            Some("Rated 8.5/10")
        );
        assert_eq!(
            sub(entry(1, ListStatus::Completed, 12, None)).as_deref(),
            Some("All 12 episodes")
        );
        assert_eq!(
            sub(entry(1, ListStatus::Watching, 3, None)).as_deref(),
            Some("Episode 3 of 12")
        );
        assert_eq!(
            sub(entry(1, ListStatus::Planning, 0, None)).as_deref(),
            Some("12 episodes")
        );
        assert_eq!(
            sub(ListEntry {
                episodes: None,
                ..entry(1, ListStatus::Planning, 0, None)
            }),
            None
        );
    }
}
