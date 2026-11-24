//! My List: the shows the user saved, newest first.

use std::sync::{Mutex, PoisonError};

use anyhow::Result;
use gpui::{
    Context, FontWeight, IntoElement, ScrollHandle, Task, WeakEntity, Window, div, prelude::*, px,
    relative,
};

use crate::app::{AppState, Root, Route, ShowRef};
use crate::db::{self, Db};
use crate::model::{Mode, ShowRecord};
use crate::ui::calendar::local_offset;
use crate::ui::components::button::{ButtonKind, button};
use crate::ui::components::poster::poster_card;
use crate::ui::metadata::CardMeta;
use crate::ui::theme;
use crate::ui::view_models::mode_label;

enum State {
    Loading,
    Ready(Vec<ShowRecord>),
    Failed(String),
}

pub struct MyListPage {
    root: WeakEntity<Root>,
    state: State,
    load_task: Option<Task<()>>,
    scroll: ScrollHandle,
}

impl MyListPage {
    pub fn new(root: WeakEntity<Root>, cx: &mut Context<Self>) -> Self {
        let mut page = MyListPage {
            root,
            state: State::Loading,
            load_task: None,
            scroll: ScrollHandle::new(),
        };
        page.reload(cx);
        page
    }

    /// Reads the list again (it changes on Show pages).
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(db) = cx.try_global::<AppState>().map(|s| s.db.clone()) else {
            return;
        };
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let loaded = cx.background_spawn(async move { load(&db) }).await;
            this.update(cx, |page, cx| {
                page.load_task = None;
                page.state = match loaded {
                    Ok(shows) => State::Ready(shows),
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

    fn render_body(&self, mode: Mode, cx: &mut Context<Self>) -> gpui::AnyElement {
        let now = db::now();
        let tz = local_offset(now);
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
            State::Ready(shows) if shows.is_empty() => div()
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
                        .child("Press + My List on any show to keep it here, ready for later."),
                )
                .child(
                    button("my-list-search", "Find a show")
                        .kind(ButtonKind::Secondary)
                        .on_click(cx.listener(|page, _, _, cx| page.navigate(Route::Search, cx))),
                )
                .into_any_element(),
            State::Ready(shows) => div()
                .flex()
                .flex_wrap()
                .gap_x(px(22.))
                .gap_y(px(30.))
                .children(shows.iter().enumerate().map(|(i, record)| {
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
                        .on_click(
                            cx.listener(move |page, _, _, cx| page.navigate(route.clone(), cx)),
                        )
                }))
                .into_any_element(),
        }
    }
}

impl Render for MyListPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = cx
            .try_global::<AppState>()
            .map_or(Mode::default(), |s| s.settings.mode);
        let count = match &self.state {
            State::Ready(shows) if !shows.is_empty() => Some(match shows.len() {
                1 => "1 show".to_string(),
                n => format!("{n} shows"),
            }),
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

/// The saved shows, newest first; ids without a stored show are skipped.
fn load(db: &Mutex<Db>) -> Result<Vec<ShowRecord>> {
    let db = db.lock().unwrap_or_else(PoisonError::into_inner);
    let mut shows = Vec::new();
    for id in db.my_list()? {
        if let Some(record) = db.get_show(&id)? {
            shows.push(record);
        }
    }
    Ok(shows)
}
