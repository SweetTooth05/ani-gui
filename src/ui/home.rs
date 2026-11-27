use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use anyhow::{Context as _, Result, anyhow};
use gpui::{
    AnyElement, App, AsyncApp, Context, FontWeight, IntoElement, ScrollHandle, SharedString, Task,
    WeakEntity, Window, div, linear_color_stop, linear_gradient, prelude::*, px, relative,
};
use serde::{Deserialize, Serialize};

use crate::app::{AppState, PlayRequest, Root, Route, ShowRef};
use crate::db::{self, Db, lock};
use crate::model::{AniListMedia, Mode, Settings, ShowRecord, ShowSummary};
use crate::ui::calendar::local_offset;
use crate::ui::components::button::{ButtonKind, button};
use crate::ui::components::image::{remote_image, rounded_image, shimmer};
use crate::ui::components::poster::{episode_card, poster_card};
use crate::ui::components::row::row;
use crate::ui::components::toast;
use crate::ui::metadata::CardMeta;
use crate::ui::theme;
use crate::ui::view_models::{
    self, Catalog, ContinueItem, HeroModel, HeroSource, format_label, media_title,
};
use crate::{allanime, catalog, history, paths};

const SNAPSHOT_KEY: &str = "home:snapshot";
const FOR_YOU_SEEDS: usize = 5;
const HERO_H: f32 = 480.;
/// About three lines of the hero's 560 px synopsis.
const HERO_SYNOPSIS_CHARS: usize = 200;
const ROW_GAP: f32 = 18.;
const ROW_OVERSCAN: usize = 2;
const DEFAULT_VIEWPORT_W: f32 = 1280.;

/// What Home shows, also cached in SQLite so the next start renders at once.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct HomeData {
    continue_items: Vec<ContinueItem>,
    trending: Vec<AniListMedia>,
    for_you: Vec<AniListMedia>,
}

impl HomeData {
    fn is_empty(&self) -> bool {
        self.continue_items.is_empty() && self.trending.is_empty() && self.for_you.is_empty()
    }
}

/// The hero's show, ready to play.
#[derive(Clone)]
struct HeroPlay {
    show: ShowSummary,
    mode: Mode,
    ep: String,
    start: f64,
    episodes: Vec<String>,
    in_list: bool,
}

struct PrecomputedHero {
    art: SharedString,
    title: SharedString,
    meta: Vec<SharedString>,
    genres: Vec<SharedString>,
    blurb: Option<SharedString>,
    play_label: SharedString,
}

struct PrecomputedContinueCard {
    title: SharedString,
    label: SharedString,
    art: SharedString,
    fraction: f32,
}

struct PrecomputedPoster {
    anilist_id: i64,
    title: SharedString,
    cover_url: SharedString,
    year: Option<SharedString>,
    meta: CardMeta,
}

#[derive(Default)]
struct PrecomputedHome {
    hero: Option<PrecomputedHero>,
    continue_cards: Vec<PrecomputedContinueCard>,
    for_you_cards: Vec<PrecomputedPoster>,
    trending_cards: Vec<PrecomputedPoster>,
}

impl PrecomputedHome {
    fn from_data(data: &HomeData, hero: Option<&HeroModel>) -> Self {
        let now = db::now();
        let tz = local_offset(now);
        let hero = hero.map(|hero| {
            let art = hero.banner_url.as_deref().unwrap_or(&hero.cover_url);
            let mut meta: Vec<SharedString> = Vec::new();
            meta.extend(hero.year.map(|y| SharedString::from(y.to_string())));
            if let HeroSource::Trending(m) = &hero.source {
                meta.extend(
                    m.format
                        .as_deref()
                        .map(|f| SharedString::from(format_label(f))),
                );
            }
            meta.extend(hero.score.map(|s| SharedString::from(format!("Score {s}"))));
            let genres = hero
                .genres
                .iter()
                .take(3)
                .map(|g| SharedString::from(g.clone()))
                .collect();
            let blurb = hero.synopsis.as_deref().map(|s| {
                SharedString::from(view_models::clamp_words(
                    &view_models::blurb(s),
                    HERO_SYNOPSIS_CHARS,
                ))
            });
            let title = match &hero.source {
                HeroSource::Trending(m) => media_title(m),
                HeroSource::Continue(item) => {
                    item.media.as_ref().map(media_title).unwrap_or(&hero.title)
                }
            };
            PrecomputedHero {
                art: SharedString::from(art.to_string()),
                title: SharedString::from(title.to_string()),
                meta,
                genres,
                blurb,
                play_label: SharedString::from(hero.play_label.clone()),
            }
        });
        let continue_cards = data
            .continue_items
            .iter()
            .map(|item| {
                let label = if item.start > 0.0 {
                    format!("Episode {}", item.ep)
                } else {
                    format!("Episode {} up next", item.ep)
                };
                let art = item.backdrop_url.as_deref().unwrap_or(&item.cover_url);
                let title = item.media.as_ref().map(media_title).unwrap_or(&item.title);
                PrecomputedContinueCard {
                    title: SharedString::from(title.to_string()),
                    label: SharedString::from(label),
                    art: SharedString::from(art.to_string()),
                    fraction: item.fraction,
                }
            })
            .collect();
        let build_posters = |items: &[AniListMedia]| -> Vec<PrecomputedPoster> {
            items
                .iter()
                .map(|media| PrecomputedPoster {
                    anilist_id: media.id,
                    title: SharedString::from(media_title(media).to_string()),
                    cover_url: SharedString::from(media.cover_url.clone()),
                    year: media.year.map(|y| SharedString::from(y.to_string())),
                    meta: CardMeta::from_media(media, now, tz),
                })
                .collect()
        };
        PrecomputedHome {
            hero,
            continue_cards,
            for_you_cards: build_posters(&data.for_you),
            trending_cards: build_posters(&data.trending),
        }
    }
}

pub struct HomePage {
    root: WeakEntity<Root>,
    data: HomeData,
    hero: Option<HeroModel>,
    hero_play: Option<HeroPlay>,
    precomputed: PrecomputedHome,
    /// `crate::model::display_generation()` when `precomputed` was built.
    display_gen: u64,
    /// A refresh has finished since the page opened.
    refreshed: bool,
    refresh_task: Option<Task<()>>,
    hero_play_task: Option<Task<()>>,
    /// Play/open in flight, with what the hero's Play button says meanwhile.
    action: Option<(Task<()>, SharedString)>,
    /// History titles no AllAnime search matched this session.
    history_misses: Arc<Mutex<HashSet<(Mode, String)>>>,
    scroll: ScrollHandle,
    rows: [ScrollHandle; 3],
}

impl HomePage {
    #[allow(dead_code)]
    pub fn new(root: WeakEntity<Root>, cx: &mut Context<Self>) -> Self {
        Self::new_with_snapshot(root, None, cx)
    }

    pub(crate) fn new_with_snapshot(
        root: WeakEntity<Root>,
        snapshot: Option<HomeData>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut page = HomePage {
            root,
            data: HomeData::default(),
            hero: None,
            hero_play: None,
            precomputed: PrecomputedHome::default(),
            display_gen: crate::model::display_generation(),
            refreshed: false,
            refresh_task: None,
            hero_play_task: None,
            action: None,
            history_misses: Arc::default(),
            scroll: ScrollHandle::new(),
            rows: Default::default(),
        };
        if let Some(data) = snapshot
            && !data.is_empty()
        {
            page.apply_data(data, cx);
        }
        page.refresh(cx);
        page
    }

    /// Shows cached content immediately (when available) and refreshes in the
    /// background, running trending and Continue Watching concurrently.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.refresh_task.is_some() {
            return;
        }
        let Some(state) = cx.try_global::<AppState>() else {
            return;
        };
        let db = state.db.clone();
        let mode = state.settings.mode;
        let misses = self.history_misses.clone();
        let cold_empty = !self.refreshed && self.data.is_empty();
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let t_load = Instant::now();
            if cold_empty {
                // Start trending and Continue Watching concurrently: trending
                // renders immediately while Continue Watching finishes in parallel.
                let db_trending = db.clone();
                let db_continue = db.clone();
                let misses_bg = misses.clone();
                let trending_task = cx.background_spawn(async move {
                    let t1 = Instant::now();
                    let trending = catalog::trending(&db_trending);
                    crate::app::trace_span("home.initial_trending", t1.elapsed());
                    trending
                });
                let continue_task = cx.background_spawn(async move {
                    let t0 = Instant::now();
                    let live = LiveCatalog { db: &db_continue };
                    let history = history::read(&paths::ani_cli_history_path());
                    let items = {
                        let mut misses = misses_bg.lock().unwrap_or_else(PoisonError::into_inner);
                        view_models::continue_items_concurrent(
                            &db_continue,
                            &history,
                            mode,
                            &live,
                            &mut misses,
                        )
                    };
                    crate::app::trace_span("home.load.continue_items", t0.elapsed());
                    items
                });
                let trending = trending_task.await;
                if !trending.is_empty() {
                    let initial = HomeData {
                        trending: trending.clone(),
                        ..HomeData::default()
                    };
                    this.update(cx, |page, cx| page.set_data(initial, cx)).ok();
                }
                let continue_items = continue_task.await;
                let db_final = db.clone();
                let data = cx
                    .background_spawn(async move {
                        let t2 = Instant::now();
                        let seeds: Vec<i64> = continue_items
                            .iter()
                            .filter_map(|i| i.media.as_ref().map(|m| m.id))
                            .take(FOR_YOU_SEEDS)
                            .collect();
                        let for_you = catalog::for_you(&db_final, &seeds);
                        crate::app::trace_span("home.load.for_you", t2.elapsed());
                        let data = HomeData {
                            continue_items,
                            trending,
                            for_you,
                        };
                        save_snapshot(&db_final, &data);
                        data
                    })
                    .await;
                crate::app::trace_span("home.load_total", t_load.elapsed());
                this.update(cx, |page, cx| {
                    page.refresh_task = None;
                    page.refreshed = true;
                    page.set_data(data, cx);
                })
                .ok();
            } else {
                let data = cx
                    .background_spawn(async move { load(&db, mode, &misses) })
                    .await;
                crate::app::trace_span("home.load_total", t_load.elapsed());
                this.update(cx, |page, cx| {
                    page.refresh_task = None;
                    page.refreshed = true;
                    page.set_data(data, cx);
                })
                .ok();
            }
        }));
        if self.data.is_empty() {
            cx.notify();
        }
    }

    fn apply_data(&mut self, data: HomeData, cx: &mut Context<Self>) {
        let hero = view_models::hero_pick(&data.continue_items, &data.trending);
        let hero_changed =
            hero.as_ref().map(|h| &h.source) != self.hero.as_ref().map(|h| &h.source);
        if hero_changed {
            self.hero_play = None;
            self.hero_play_task = None;
        }
        self.precomputed = PrecomputedHome::from_data(&data, hero.as_ref());
        self.display_gen = crate::model::display_generation();
        self.hero = hero;
        self.data = data;
        if self.hero.is_some() && self.hero_play.is_none() && self.hero_play_task.is_none() {
            self.prepare_hero_play(cx);
        }
    }

    fn set_data(&mut self, data: HomeData, cx: &mut Context<Self>) {
        if data == self.data {
            if self.hero.is_some() && self.hero_play.is_none() && self.hero_play_task.is_none() {
                self.prepare_hero_play(cx);
            }
            return;
        }
        self.apply_data(data, cx);
        cx.notify();
    }

    /// Resolves `hero_play` off the critical path so My List and Play are ready
    /// without delaying Home content updates.
    fn prepare_hero_play(&mut self, cx: &mut Context<Self>) {
        let (Some(hero), Some(state)) = (self.hero.clone(), cx.try_global::<AppState>()) else {
            return;
        };
        let db = state.db.clone();
        let mode = state.settings.mode;
        let expected_source = hero.source.clone();
        self.hero_play_task = Some(cx.spawn(async move |this, cx| {
            let t0 = Instant::now();
            let resolved = cx
                .background_spawn(async move {
                    let live = LiveCatalog { db: &db };
                    hero_play(&db, &live, &hero, mode)
                        .inspect_err(|err| {
                            eprintln!("ani-gui: preparing {:?}: {err:#}", hero.title)
                        })
                        .ok()
                })
                .await;
            crate::app::trace_span("home.load.hero_play", t0.elapsed());
            this.update(cx, |page, cx| {
                page.hero_play_task = None;
                if page.hero.as_ref().map(|h| &h.source) == Some(&expected_source) {
                    page.hero_play = resolved;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn navigate(&self, route: Route, cx: &mut Context<Self>) {
        self.root
            .update(cx, |root, cx| root.navigate(route, cx))
            .ok();
    }

    /// Runs `find` in the background, then plays what it returns.
    fn start_play(
        &mut self,
        label: &'static str,
        find: impl FnOnce(&Mutex<Db>) -> Result<PlayRequest> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        if self.action.is_some() {
            return;
        }
        let Some(db) = cx.try_global::<AppState>().map(|s| s.db.clone()) else {
            return;
        };
        let task = cx.spawn(async move |this, cx: &mut AsyncApp| {
            let found = cx.background_spawn(async move { find(&db) }).await;
            this.update(cx, |page, cx| {
                page.action = None;
                cx.notify();
                match found {
                    Ok(req) => {
                        let from = cx.entity_id();
                        page.root
                            .update(cx, |root, cx| root.play_from(from, req, cx))
                            .ok();
                    }
                    Err(err) => toast::error(cx, format!("Couldn't start playback: {err:#}")),
                }
            })
            .ok();
        });
        self.action = Some((task, label.into()));
        cx.notify();
    }

    fn play_hero(&mut self, cx: &mut Context<Self>) {
        let Some(hero) = self.hero.clone() else {
            return;
        };
        let ready = self.hero_play.clone();
        let mode = cx
            .try_global::<AppState>()
            .map_or(Mode::default(), |s| s.settings.mode);
        let quality = settings_quality(cx);
        self.start_play(
            "Finding episodes…",
            move |db| {
                let play = match ready {
                    Some(play) => play,
                    None => hero_play(db, &LiveCatalog { db }, &hero, mode)?,
                };
                Ok(PlayRequest {
                    show: play.show,
                    mode: play.mode,
                    ep: play.ep,
                    start: play.start,
                    episodes: play.episodes,
                    quality,
                    local_file: None,
                    stream: None,
                })
            },
            cx,
        );
    }

    fn play_continue_index(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(item) = self.data.continue_items.get(index).cloned() else {
            return;
        };
        self.play_item(item, cx);
    }

    fn play_item(&mut self, item: ContinueItem, cx: &mut Context<Self>) {
        let quality = settings_quality(cx);
        self.start_play(
            "Opening…",
            move |db| {
                let show = lock(db)
                    .get_show(&item.show_id)?
                    .context("this show is no longer in the library")?
                    .summary;
                let episodes = (LiveCatalog { db }).episodes(&show, item.mode);
                Ok(PlayRequest {
                    show,
                    mode: item.mode,
                    ep: item.ep,
                    start: item.start,
                    episodes,
                    quality,
                    local_file: None,
                    stream: None,
                })
            },
            cx,
        );
    }

    fn toggle_hero_list(&mut self, cx: &mut Context<Self>) {
        let (Some(play), Some(db)) = (
            self.hero_play.as_mut(),
            cx.try_global::<AppState>().map(|s| s.db.clone()),
        ) else {
            return;
        };
        let add = !play.in_list;
        play.in_list = add;
        let id = play.show.id.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let saved = cx
                .background_spawn(async move {
                    let db = lock(&db);
                    if add {
                        db.my_list_add(&id)
                    } else {
                        db.my_list_remove(&id)
                    }
                })
                .await;
            if let Err(err) = saved {
                this.update(cx, |page, cx| {
                    if let Some(play) = page.hero_play.as_mut() {
                        play.in_list = !add;
                    }
                    toast::error(cx, format!("Couldn't update My List: {err:#}"));
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn open_hero_details(&mut self, cx: &mut Context<Self>) {
        let Some(hero) = &self.hero else {
            return;
        };
        let anilist_id = match &hero.source {
            HeroSource::Continue(item) => item.media.as_ref().map(|m| m.id),
            HeroSource::Trending(media) => Some(media.id),
        };
        let title = self
            .precomputed
            .hero
            .as_ref()
            .map(|h| h.title.to_string())
            .unwrap_or_else(|| hero.title.clone());
        let route = Route::Show(ShowRef {
            allanime: self.hero_play.as_ref().map(|p| p.show.clone()),
            anilist_id,
            title,
        });
        self.navigate(route, cx);
    }

    fn hero_actions(&self, hero: &PrecomputedHero, cx: &mut Context<Self>) -> gpui::Div {
        let busy = self.action.as_ref().map(|(_, label)| label.clone());
        let in_list = self.hero_play.as_ref().map(|p| p.in_list);
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .pt(px(6.))
            .child(
                button(
                    "hero-play",
                    busy.clone().unwrap_or_else(|| hero.play_label.clone()),
                )
                .disabled(busy.is_some())
                .on_click(cx.listener(|page, _, _, cx| page.play_hero(cx))),
            )
            .child(
                button(
                    "hero-list",
                    if in_list == Some(true) {
                        "In My List"
                    } else {
                        "+ My List"
                    },
                )
                .kind(ButtonKind::Secondary)
                .disabled(in_list.is_none())
                .on_click(cx.listener(|page, _, _, cx| page.toggle_hero_list(cx))),
            )
            .child(
                button("hero-details", "Details")
                    .kind(ButtonKind::Ghost)
                    .on_click(cx.listener(|page, _, _, cx| page.open_hero_details(cx))),
            )
    }

    fn render_hero(&self, hero: &PrecomputedHero, cx: &mut Context<Self>) -> AnyElement {
        if theme::is_light() {
            return self.render_hero_light(hero, cx);
        }
        div()
            .relative()
            .w_full()
            .h(px(HERO_H))
            .flex_none()
            .overflow_hidden()
            .child(div().absolute().inset_0().child(remote_image(&hero.art)))
            .child(div().absolute().inset_0().bg(linear_gradient(
                90.,
                linear_color_stop(theme::night_alpha(0.97), 0.),
                linear_color_stop(theme::night_alpha(0.), 0.72),
            )))
            .child(div().absolute().inset_0().bg(linear_gradient(
                0.,
                linear_color_stop(theme::night_alpha(1.), 0.),
                linear_color_stop(theme::night_alpha(0.), 0.5),
            )))
            .child(
                div()
                    .absolute()
                    .left(theme::gutter())
                    .bottom(px(40.))
                    .w(px(600.))
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .child(
                        div()
                            .font_family(theme::DISPLAY_FONT)
                            .font_weight(FontWeight::BOLD)
                            .text_size(px(theme::HERO_TITLE))
                            .line_height(relative(0.95))
                            .text_color(theme::text())
                            .line_clamp(2)
                            .child(hero.title.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(14.))
                            .text_size(px(theme::SMALL))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text_dim())
                            .children(hero.meta.iter().cloned().map(|m| div().child(m)))
                            .children(hero.genres.iter().cloned().map(|g| {
                                div()
                                    .px(px(10.))
                                    .py(px(3.))
                                    .rounded(px(6.))
                                    .border_1()
                                    .border_color(gpui::white().opacity(0.18))
                                    .child(g)
                            })),
                    )
                    .children(hero.blurb.clone().map(|s| {
                        div()
                            .max_w(px(560.))
                            .text_size(px(theme::BODY))
                            .line_height(relative(1.5))
                            .text_color(theme::text_dim())
                            .line_clamp(3)
                            .child(s)
                    }))
                    .child(self.hero_actions(hero, cx)),
            )
            .into_any_element()
    }

    /// Light-first editorial hero: pure-white card with crisp dark typography
    /// beside un-faded, gallery-framed artwork so banner art never looks milky.
    fn render_hero_light(&self, hero: &PrecomputedHero, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex_none()
            .w_full()
            .px(theme::gutter())
            .pt(px(28.))
            .pb(px(16.))
            .child(
                div()
                    .w_full()
                    .h(px(348.))
                    .p(px(12.))
                    .rounded(px(18.))
                    .bg(theme::control())
                    .border_1()
                    .border_color(theme::line())
                    .shadow(theme::shadow_md())
                    .flex()
                    .gap(px(24.))
                    .child(
                        div()
                            .w(relative(0.47))
                            .min_w(px(360.))
                            .max_w(px(520.))
                            .h_full()
                            .flex_none()
                            .flex()
                            .flex_col()
                            .justify_between()
                            .py(px(18.))
                            .pl(px(22.))
                            .pr(px(8.))
                            .overflow_hidden()
                            .child(
                                div()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap(px(12.))
                                    .child(
                                        div()
                                            .font_family(theme::DISPLAY_FONT)
                                            .font_weight(FontWeight::BOLD)
                                            .text_size(px(40.))
                                            .line_height(relative(1.0))
                                            .text_color(theme::text())
                                            .line_clamp(2)
                                            .child(hero.title.clone()),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .flex_wrap()
                                            .items_center()
                                            .gap_x(px(10.))
                                            .gap_y(px(6.))
                                            .text_size(px(theme::SMALL))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(theme::text_dim())
                                            .children(
                                                hero.meta.iter().cloned().map(|m| div().child(m)),
                                            )
                                            .children(hero.genres.iter().cloned().map(|g| {
                                                div()
                                                    .px(px(9.))
                                                    .py(px(2.))
                                                    .rounded_full()
                                                    .bg(theme::night())
                                                    .border_1()
                                                    .border_color(theme::line())
                                                    .text_size(px(12.))
                                                    .font_weight(FontWeight::MEDIUM)
                                                    .text_color(theme::text_dim())
                                                    .child(g)
                                            })),
                                    )
                                    .children(hero.blurb.clone().map(|s| {
                                        div()
                                            .text_size(px(14.5))
                                            .line_height(relative(1.52))
                                            .text_color(theme::text_dim())
                                            .line_clamp(3)
                                            .child(s)
                                    })),
                            )
                            .child(self.hero_actions(hero, cx)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .rounded(px(12.))
                            .overflow_hidden()
                            .child(rounded_image(&hero.art, px(12.), theme::control(), None)),
                    ),
            )
            .into_any_element()
    }

    fn media_row(
        &self,
        id: &'static str,
        title: &'static str,
        items: &[PrecomputedPoster],
        scroll: &ScrollHandle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (start, end, leading_w, trailing_w) =
            horizontal_window(items.len(), theme::POSTER_W, scroll);
        let mut children: Vec<AnyElement> = Vec::with_capacity(
            (end - start) + usize::from(leading_w > 0.) + usize::from(trailing_w > 0.),
        );
        if leading_w > 0. {
            children.push(horizontal_spacer(leading_w));
        }
        for (i, item) in items[start..end].iter().enumerate() {
            let idx = start + i;
            let anilist_id = item.anilist_id;
            let route_title = item.title.clone();
            let card = poster_card((id, idx), item.title.clone(), &item.cover_url)
                .meta(item.meta.clone())
                .on_click(cx.listener(move |page, _, _, cx| {
                    page.navigate(
                        Route::Show(ShowRef {
                            allanime: None,
                            anilist_id: Some(anilist_id),
                            title: route_title.to_string(),
                        }),
                        cx,
                    );
                }));
            let card = match &item.year {
                Some(year) => card.subtitle(year.clone()),
                None => card,
            };
            children.push(card.into_any_element());
        }
        if trailing_w > 0. {
            children.push(horizontal_spacer(trailing_w));
        }
        row(id, title, scroll, children).into_any_element()
    }

    fn continue_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let items = &self.precomputed.continue_cards;
        let (start, end, leading_w, trailing_w) =
            horizontal_window(items.len(), theme::CARD_W, &self.rows[0]);
        let mut children: Vec<AnyElement> = Vec::with_capacity(
            (end - start) + usize::from(leading_w > 0.) + usize::from(trailing_w > 0.),
        );
        if leading_w > 0. {
            children.push(horizontal_spacer(leading_w));
        }
        for (i, item) in items[start..end].iter().enumerate() {
            let idx = start + i;
            children.push(
                episode_card(
                    ("continue", idx),
                    item.title.clone(),
                    item.label.clone(),
                    &item.art,
                    item.fraction,
                )
                .on_click(cx.listener(move |page, _, _, cx| page.play_continue_index(idx, cx)))
                .into_any_element(),
            );
        }
        if trailing_w > 0. {
            children.push(horizontal_spacer(trailing_w));
        }
        row("continue", "Continue watching", &self.rows[0], children).into_any_element()
    }
}

/// Computes the visible `[start, end)` slice of a fixed-width horizontal card
/// row plus leading/trailing spacer widths (accounting for `row()`'s `ROW_GAP`).
fn horizontal_window(count: usize, card_w: f32, scroll: &ScrollHandle) -> (usize, usize, f32, f32) {
    if count == 0 {
        return (0, 0, 0., 0.);
    }
    let stride = card_w + ROW_GAP;
    let scroll_x = (-f32::from(scroll.offset().x)).max(0.);
    let viewport_w = {
        let measured = f32::from(scroll.bounds().size.width);
        if measured > 1.0 {
            measured
        } else {
            DEFAULT_VIEWPORT_W
        }
    };
    let first_visible = (scroll_x / stride).floor() as usize;
    let start = first_visible
        .saturating_sub(ROW_OVERSCAN)
        .min(count.saturating_sub(1));
    let visible = (viewport_w / stride).ceil() as usize + 1 + ROW_OVERSCAN * 2;
    let end = (start + visible).min(count);
    let leading_w = if start > 0 {
        (start as f32 * stride - ROW_GAP).max(0.)
    } else {
        0.
    };
    let trailing_n = count - end;
    let trailing_w = if trailing_n > 0 {
        (trailing_n as f32 * stride - ROW_GAP).max(0.)
    } else {
        0.
    };
    (start, end, leading_w, trailing_w)
}

fn horizontal_spacer(width: f32) -> AnyElement {
    div().flex_none().w(px(width)).h(px(1.)).into_any_element()
}

pub fn skeleton() -> AnyElement {
    let hero_skel = if theme::is_light() {
        div().w_full().px(theme::gutter()).pt(px(28.)).child(
            div()
                .w_full()
                .h(px(348.))
                .rounded(px(18.))
                .overflow_hidden()
                .border_1()
                .border_color(theme::line())
                .child(shimmer()),
        )
    } else {
        div().w_full().h(px(HERO_H)).child(shimmer())
    };
    div()
        .flex()
        .flex_col()
        .gap(px(40.))
        .child(hero_skel)
        .child(
            div()
                .flex()
                .gap(px(18.))
                .px(theme::gutter())
                .children((0..6usize).map(|i| {
                    div()
                        .id(("home-skeleton", i))
                        .flex_none()
                        .w(px(theme::POSTER_W))
                        .h(px(theme::POSTER_H))
                        .rounded(px(10.))
                        .bg(theme::raised())
                })),
        )
        .into_any_element()
}

impl Render for HomePage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let trace_start = crate::app::trace_enabled().then(Instant::now);
        let display_gen = crate::model::display_generation();
        if display_gen != self.display_gen {
            self.display_gen = display_gen;
            if !self.data.is_empty() {
                self.hero = view_models::hero_pick(&self.data.continue_items, &self.data.trending);
                self.precomputed = PrecomputedHome::from_data(&self.data, self.hero.as_ref());
            }
        }
        static FIRST_RENDER: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        static FIRST_CONTENT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        if trace_start.is_some() {
            if FIRST_RENDER.set(()).is_ok() {
                crate::app::trace_event("home_first_render");
            }
            if !self.data.is_empty() && FIRST_CONTENT.set(()).is_ok() {
                crate::app::trace_event("home_first_content_render");
            }
        }
        let body: AnyElement = if self.data.is_empty() {
            if self.refreshed {
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap_4()
                    .px(theme::gutter())
                    .pt(px(160.))
                    .child(
                        div()
                            .font_family(theme::DISPLAY_FONT)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(px(32.))
                            .text_color(theme::text())
                            .child("Couldn't reach AniList — check your connection"),
                    )
                    .child(
                        div().child(
                            button(
                                "home-retry",
                                if self.refresh_task.is_some() {
                                    "Trying again…"
                                } else {
                                    "Try again"
                                },
                            )
                            .kind(ButtonKind::Secondary)
                            .disabled(self.refresh_task.is_some())
                            .on_click(cx.listener(|page, _, _, cx| page.refresh(cx))),
                        ),
                    )
                    .into_any_element()
            } else {
                skeleton()
            }
        } else {
            let hero = self
                .precomputed
                .hero
                .as_ref()
                .map(|hero| self.render_hero(hero, cx));
            let mut rows: Vec<AnyElement> = Vec::with_capacity(3);
            if !self.precomputed.continue_cards.is_empty() {
                rows.push(self.continue_row(cx));
            }
            if !self.precomputed.for_you_cards.is_empty() {
                rows.push(self.media_row(
                    "for-you",
                    "For you",
                    &self.precomputed.for_you_cards,
                    &self.rows[1],
                    cx,
                ));
            }
            if !self.precomputed.trending_cards.is_empty() {
                rows.push(self.media_row(
                    "trending",
                    "Trending now",
                    &self.precomputed.trending_cards,
                    &self.rows[2],
                    cx,
                ));
            }
            div()
                .flex()
                .flex_col()
                .children(hero)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(40.))
                        .pt(px(12.))
                        .pb(px(64.))
                        .children(rows),
                )
                .into_any_element()
        };
        let mut page = div()
            .id("home")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .child(body);
        page.style().restrict_scroll_to_axis = Some(true);
        if let Some(t) = trace_start {
            crate::app::trace_span("home.render", t.elapsed());
        }
        page
    }
}

/// AllAnime and AniList lookups for Continue Watching, backed by SQLite's
/// persistent episode and metadata caches.
struct LiveCatalog<'a> {
    db: &'a Mutex<Db>,
}

impl Catalog for LiveCatalog<'_> {
    fn search(&self, title: &str, mode: Mode) -> Result<Vec<ShowSummary>> {
        allanime::search(title, mode)
    }

    fn episodes(&self, show: &ShowSummary, mode: Mode) -> Vec<String> {
        match catalog::episodes(self.db, &show.id, mode) {
            Ok(eps) => eps,
            Err(err) => {
                eprintln!("ani-gui: episodes of {}: {err:#}", show.id);
                Vec::new()
            }
        }
    }

    fn media(&self, anilist_id: i64) -> Option<AniListMedia> {
        catalog::media(self.db, anilist_id)
    }
}

fn settings_quality(cx: &App) -> String {
    cx.try_global::<AppState>().map_or_else(
        || Settings::default().quality,
        |s| s.settings.quality.clone(),
    )
}

pub(crate) fn read_snapshot_db(db: &Db) -> Option<HomeData> {
    let json = db.cache_get_stale(SNAPSHOT_KEY).ok().flatten()?;
    serde_json::from_str(&json)
        .inspect_err(|err| eprintln!("ani-gui: cached home is unreadable: {err}"))
        .ok()
}

fn save_snapshot(db: &Mutex<Db>, data: &HomeData) {
    if data.is_empty() {
        return;
    }
    let stored = serde_json::to_string(data)
        .map_err(anyhow::Error::from)
        .and_then(|json| lock(db).cache_put(SNAPSHOT_KEY, &json));
    if let Err(err) = stored {
        eprintln!("ani-gui: caching home: {err:#}");
    }
}

/// Everything Home shows, fetched concurrently (catalog calls use their own
/// SQLite caches). `hero_play` runs separately so it never blocks row updates.
fn load(db: &Mutex<Db>, mode: Mode, misses: &Mutex<HashSet<(Mode, String)>>) -> HomeData {
    let live = LiveCatalog { db };
    let (continue_items, trending) = std::thread::scope(|s| {
        let trending_handle = s.spawn(|| {
            let t1 = Instant::now();
            let trending = catalog::trending(db);
            crate::app::trace_span("home.load.trending", t1.elapsed());
            trending
        });
        let t0 = Instant::now();
        let history = history::read(&paths::ani_cli_history_path());
        let continue_items = {
            let mut misses = misses.lock().unwrap_or_else(PoisonError::into_inner);
            view_models::continue_items_concurrent(db, &history, mode, &live, &mut misses)
        };
        crate::app::trace_span("home.load.continue_items", t0.elapsed());
        let trending = trending_handle.join().unwrap_or_default();
        (continue_items, trending)
    });
    let t2 = Instant::now();
    let seeds: Vec<i64> = continue_items
        .iter()
        .filter_map(|i| i.media.as_ref().map(|m| m.id))
        .take(FOR_YOU_SEEDS)
        .collect();
    let for_you = catalog::for_you(db, &seeds);
    crate::app::trace_span("home.load.for_you", t2.elapsed());
    let data = HomeData {
        continue_items,
        trending,
        for_you,
    };
    save_snapshot(db, &data);
    data
}

fn hero_play(
    db: &Mutex<Db>,
    live: &LiveCatalog<'_>,
    hero: &HeroModel,
    mode: Mode,
) -> Result<HeroPlay> {
    let (show, mode, ep, start, episodes) = match &hero.source {
        HeroSource::Continue(item) => {
            let show = lock(db)
                .get_show(&item.show_id)?
                .context("this show is no longer in the library")?
                .summary;
            let episodes = live.episodes(&show, item.mode);
            (show, item.mode, item.ep.clone(), item.start, episodes)
        }
        HeroSource::Trending(media) => {
            // Sub lists nearly everything; a show without dub episodes then
            // reports that below instead of looking missing.
            let show = catalog::resolve_allanime_cached(db, media, Mode::Sub)?
                .ok_or_else(|| anyhow!("{} isn't on AllAnime", media_title(media)))?;
            lock(db).upsert_show(&ShowRecord {
                summary: show.clone(),
                media: Some(media.clone()),
                updated_at: db::now(),
            })?;
            let episodes = live.episodes(&show, mode);
            let first = episodes
                .first()
                .cloned()
                .ok_or_else(|| anyhow!("no {mode} episodes yet"))?;
            (show, mode, first, 0.0, episodes)
        }
    };
    let in_list = lock(db).my_list_contains(&show.id)?;
    Ok(HeroPlay {
        show,
        mode,
        ep,
        start,
        episodes,
        in_list,
    })
}
