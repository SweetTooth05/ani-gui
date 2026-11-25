use std::collections::HashMap;
use std::time::Duration;

use gpui::{
    AnyElement, Context, Entity, Focusable, FontWeight, IntoElement, ScrollHandle, SharedString,
    Subscription, Task, WeakEntity, Window, div, prelude::*, px,
};

use crate::app::{AppState, Root, Route, ShowRef};
use crate::db;
use crate::model::{AniListMedia, Mode, ShowSummary};
use crate::ui::calendar::local_offset;
use crate::ui::components::button::segmented;
use crate::ui::components::poster::poster_card;
use crate::ui::components::text_input::{Changed, TextInput};
use crate::ui::metadata::CardMeta;
use crate::ui::theme;
use crate::{allanime, catalog};

const DEBOUNCE: Duration = Duration::from_millis(300);
const GRID_GAP_X: f32 = 22.;
const GRID_ROW_H: f32 = 308.;
const GRID_ROW_GAP: f32 = 30.;
const GRID_ROW_STRIDE: f32 = GRID_ROW_H + GRID_ROW_GAP;
/// Approximate y-offset of the results grid below the heading and search bar.
const GRID_TOP_Y: f32 = 190.;

struct ResultCard {
    title: SharedString,
    cover_url: String,
    meta: CardMeta,
    subtitle: SharedString,
}

enum State {
    Idle,
    Searching,
    Done,
    Failed(String),
}

pub struct SearchPage {
    root: WeakEntity<Root>,
    input: Entity<TextInput>,
    mode: Mode,
    /// The query `results` are for.
    query: String,
    results: Vec<ShowSummary>,
    /// AniList card data for `results` that carry an AniList id, fetched
    /// after them so results appear without waiting on AniList.
    media: HashMap<i64, AniListMedia>,
    /// Precomputed display cards for `results` + `media`.
    cards: Vec<ResultCard>,
    state: State,
    pending: Option<Task<()>>,
    enrich: Option<Task<()>>,
    focus_next_render: bool,
    scroll: ScrollHandle,
    _changes: Subscription,
}

impl SearchPage {
    pub fn new(root: WeakEntity<Root>, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextInput::new("Search by English or Japanese title", cx));
        let changes = cx.subscribe(&input, |page, _, _: &Changed, cx| {
            page.schedule(DEBOUNCE, cx)
        });
        let mode = cx
            .try_global::<AppState>()
            .map_or(Mode::default(), |s| s.settings.mode);
        SearchPage {
            root,
            input,
            mode,
            query: String::new(),
            results: Vec::new(),
            media: HashMap::new(),
            cards: Vec::new(),
            state: State::Idle,
            pending: None,
            enrich: None,
            focus_next_render: true,
            scroll: ScrollHandle::new(),
            _changes: changes,
        }
    }

    pub fn focus_on_next_render(&mut self) {
        self.focus_next_render = true;
    }

    fn rebuild_cards(&mut self) {
        let now = db::now();
        let tz = local_offset(now);
        let mode = self.mode;
        self.cards = self
            .results
            .iter()
            .map(|show| {
                let count = show.count(mode);
                let media = show.anilist_id.and_then(|id| self.media.get(&id));
                let meta = match media {
                    Some(media) => CardMeta::from_media(media, now, tz),
                    None => CardMeta::from_summary(show),
                };
                let subtitle: SharedString = if count == 1 {
                    "1 episode".into()
                } else {
                    format!("{count} episodes").into()
                };
                ResultCard {
                    title: show.display_title().to_string().into(),
                    cover_url: show.cover_url.clone(),
                    meta,
                    subtitle,
                }
            })
            .collect();
    }

    fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if self.mode != mode {
            self.mode = mode;
            self.schedule(Duration::ZERO, cx);
        }
    }

    /// Searches for the input's text after `delay`, replacing any search
    /// still waiting or running.
    fn schedule(&mut self, delay: Duration, cx: &mut Context<Self>) {
        let query = self.input.read(cx).text().trim().to_string();
        let mode = self.mode;
        if query.is_empty() {
            self.pending = None;
            self.query.clear();
            self.results.clear();
            self.cards.clear();
            self.state = State::Idle;
            cx.notify();
            return;
        }
        self.pending = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |page, cx| {
                page.state = State::Searching;
                cx.notify();
            })
            .ok();
            let q = query.clone();
            let found = cx
                .background_spawn(async move { allanime::search(&q, mode) })
                .await;
            this.update(cx, |page, cx| {
                page.pending = None;
                page.query = query;
                match found {
                    Ok(results) => {
                        page.results = results;
                        page.rebuild_cards();
                        page.state = State::Done;
                        page.enrich(cx);
                    }
                    Err(err) => {
                        page.results.clear();
                        page.cards.clear();
                        page.state = State::Failed(format!("{err:#}"));
                    }
                }
                page.scroll.set_offset(gpui::point(px(0.), px(0.)));
                cx.notify();
            })
            .ok();
        }));
    }

    /// Fetches AniList card data for the results in the background.
    fn enrich(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<i64> = self
            .results
            .iter()
            .filter_map(|s| s.anilist_id)
            .filter(|id| !self.media.contains_key(id))
            .collect();
        let Some(db) = cx.try_global::<AppState>().map(|s| s.db.clone()) else {
            return;
        };
        if ids.is_empty() {
            return;
        }
        self.enrich = Some(cx.spawn(async move |this, cx| {
            let found = cx
                .background_spawn(async move { catalog::cards(&db, &ids) })
                .await;
            this.update(cx, |page, cx| {
                page.enrich = None;
                if !found.is_empty() {
                    page.media.extend(found.into_iter().map(|m| (m.id, m)));
                    page.rebuild_cards();
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn open_index(&self, index: usize, cx: &mut Context<Self>) {
        let Some(show) = self.results.get(index).cloned() else {
            return;
        };
        let route = Route::Show(ShowRef {
            anilist_id: show.anilist_id,
            title: show.display_title().to_string(),
            allanime: Some(show),
        });
        self.root
            .update(cx, |root, cx| root.navigate(route, cx))
            .ok();
    }

    fn status_line(&self) -> Option<String> {
        match &self.state {
            State::Idle => None,
            State::Searching => Some("Searching…".into()),
            State::Failed(err) => Some(format!("Search failed: {err}")),
            State::Done if self.results.is_empty() => Some(format!(
                "No {} results for \u{201c}{}\u{201d}. Try the other title, or switch to {}.",
                self.mode,
                self.query,
                match self.mode {
                    Mode::Sub => "Dub",
                    Mode::Dub => "Sub",
                }
            )),
            State::Done => None,
        }
    }

    fn render_grid(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let count = self.cards.len();
        if count == 0 {
            return div().into_any_element();
        }
        let bounds = self.scroll.bounds();
        let mut vw = f32::from(bounds.size.width);
        let mut vh = f32::from(bounds.size.height);
        if vw <= 1.0 {
            vw = (f32::from(window.viewport_size().width) - theme::SIDEBAR_WIDTH)
                .max(theme::POSTER_W + 2.0 * f32::from(theme::gutter()));
        }
        if vh <= 1.0 {
            vh = f32::from(window.viewport_size().height).max(720.0);
        }
        let avail = (vw - 2.0 * f32::from(theme::gutter())).max(theme::POSTER_W);
        let cols = ((avail + GRID_GAP_X) / (theme::POSTER_W + GRID_GAP_X))
            .floor()
            .max(1.0) as usize;
        let total_rows = count.div_ceil(cols);
        let scroll_y = (-f32::from(self.scroll.offset().y)).max(0.0);
        let vis_top = (scroll_y - GRID_TOP_Y - GRID_ROW_STRIDE).max(0.0);
        let vis_bottom = (scroll_y + vh - GRID_TOP_Y + GRID_ROW_STRIDE).max(0.0);
        let start_row = ((vis_top / GRID_ROW_STRIDE).floor() as usize).min(total_rows);
        let end_row = ((vis_bottom / GRID_ROW_STRIDE).ceil() as usize).min(total_rows);
        let top_h = start_row as f32 * GRID_ROW_STRIDE;
        let bottom_h = (total_rows - end_row) as f32 * GRID_ROW_STRIDE;

        let mut container = div().flex().flex_col();
        if top_h > 0.0 {
            container = container.child(div().h(px(top_h)));
        }
        for row in start_row..end_row {
            let first = row * cols;
            let last = (first + cols).min(count);
            container = container.child(
                div()
                    .flex()
                    .gap(px(GRID_GAP_X))
                    .h(px(GRID_ROW_H))
                    .mb(px(GRID_ROW_GAP))
                    .children((first..last).map(|i| {
                        let card = &self.cards[i];
                        poster_card(("result", i), card.title.clone(), &card.cover_url)
                            .meta(card.meta.clone())
                            .subtitle(card.subtitle.clone())
                            .on_click(cx.listener(move |page, _, _, cx| page.open_index(i, cx)))
                    })),
            );
        }
        if bottom_h > 0.0 {
            container = container.child(div().h(px(bottom_h)));
        }
        container.into_any_element()
    }
}

impl Render for SearchPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.focus_next_render {
            self.focus_next_render = false;
            window.focus(&self.input.focus_handle(cx));
        }
        let mode = self.mode;
        let failed = matches!(self.state, State::Failed(_));
        div()
            .id("search")
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
                            .font_family(theme::DISPLAY_FONT)
                            .font_weight(FontWeight::BOLD)
                            .text_size(px(theme::PAGE_TITLE))
                            .line_height(px(theme::PAGE_TITLE))
                            .child("Search"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .child(div().flex_1().max_w(px(720.)).child(self.input.clone()))
                            .child(
                                segmented("search-mode")
                                    .option(
                                        "Sub",
                                        mode == Mode::Sub,
                                        cx.listener(|page, _, _, cx| page.set_mode(Mode::Sub, cx)),
                                    )
                                    .option(
                                        "Dub",
                                        mode == Mode::Dub,
                                        cx.listener(|page, _, _, cx| page.set_mode(Mode::Dub, cx)),
                                    ),
                            ),
                    )
                    .children(self.status_line().map(|line| {
                        div()
                            .text_size(px(theme::BODY))
                            .text_color(if failed {
                                theme::danger()
                            } else {
                                theme::text_dim()
                            })
                            .child(line)
                    }))
                    .child(self.render_grid(window, cx)),
            )
    }
}
