use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use gpui::{
    AnyElement, App, AsyncApp, Context, FontWeight, IntoElement, ScrollHandle, SharedString, Task,
    WeakEntity, Window, div, linear_color_stop, linear_gradient, prelude::*, px, relative,
};
use serde::{Deserialize, Serialize};

use crate::app::{AppState, PlayRequest, Root, Route, ShowRef};
use crate::db::{self, Db};
use crate::model::{AniListMedia, Mode, Settings, ShowRecord, ShowSummary};
use crate::ui::components::button::{ButtonKind, button};
use crate::ui::components::image::{remote_image, shimmer};
use crate::ui::components::poster::{episode_card, poster_card};
use crate::ui::components::row::row;
use crate::ui::components::toast;
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
/// AllAnime episode lists are reused this long within a session.
const EPISODES_TTL: Duration = Duration::from_secs(10 * 60);

/// What Home shows, also cached in SQLite so the next start renders at once.
#[derive(Clone, Default, Serialize, Deserialize)]
struct HomeData {
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

pub struct HomePage {
    root: WeakEntity<Root>,
    data: HomeData,
    hero: Option<HeroModel>,
    hero_play: Option<HeroPlay>,
    /// A refresh has finished since the page opened.
    refreshed: bool,
    refresh_task: Option<Task<()>>,
    /// Play/open in flight, with what the hero's Play button says meanwhile.
    action: Option<(Task<()>, SharedString)>,
    /// History titles no AllAnime search matched this session.
    history_misses: Arc<Mutex<HashSet<(Mode, String)>>>,
    scroll: ScrollHandle,
    rows: [ScrollHandle; 3],
}

impl HomePage {
    pub fn new(root: WeakEntity<Root>, cx: &mut Context<Self>) -> Self {
        let mut page = HomePage {
            root,
            data: HomeData::default(),
            hero: None,
            hero_play: None,
            refreshed: false,
            refresh_task: None,
            action: None,
            history_misses: Arc::default(),
            scroll: ScrollHandle::new(),
            rows: Default::default(),
        };
        page.refresh(cx);
        page
    }

    /// Shows the cached snapshot (first time only), then reloads everything
    /// in the background.
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
        let first = !self.refreshed && self.data.is_empty();
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            if first {
                let (db, db2) = (db.clone(), db.clone());
                let cached = cx
                    .background_spawn(async move { read_snapshot(&db2) })
                    .await;
                let data = match cached {
                    Some(data) => data,
                    // Nothing cached yet: trending is quick, Continue Watching is not.
                    None => HomeData {
                        trending: cx
                            .background_spawn(async move { catalog::trending(&db) })
                            .await,
                        ..HomeData::default()
                    },
                };
                this.update(cx, |page, cx| page.set_data(data, cx)).ok();
            }
            let (data, hero_play) = cx
                .background_spawn(async move { load(&db, mode, &misses) })
                .await;
            this.update(cx, |page, cx| {
                page.refresh_task = None;
                page.refreshed = true;
                page.set_data(data, cx);
                page.hero_play = hero_play;
            })
            .ok();
        }));
        cx.notify();
    }

    fn set_data(&mut self, data: HomeData, cx: &mut Context<Self>) {
        let hero = view_models::hero_pick(&data.continue_items, &data.trending);
        if hero.as_ref().map(|h| &h.source) != self.hero.as_ref().map(|h| &h.source) {
            self.hero_play = None;
        }
        self.hero = hero;
        self.data = data;
        cx.notify();
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
                    Ok(req) => page.navigate(Route::Player(req), cx),
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
        let route = Route::Show(ShowRef {
            allanime: self.hero_play.as_ref().map(|p| p.show.clone()),
            anilist_id,
            title: hero.title.clone(),
        });
        self.navigate(route, cx);
    }

    fn render_hero(&self, hero: &HeroModel, cx: &mut Context<Self>) -> AnyElement {
        let art = hero.banner_url.as_deref().unwrap_or(&hero.cover_url);
        let mut meta: Vec<String> = Vec::new();
        meta.extend(hero.year.map(|y| y.to_string()));
        if let HeroSource::Trending(m) = &hero.source {
            meta.extend(m.format.as_deref().map(format_label));
        }
        meta.extend(hero.score.map(|s| format!("Score {s}")));
        let busy = self.action.as_ref().map(|(_, label)| label.clone());
        let in_list = self.hero_play.as_ref().map(|p| p.in_list);
        div()
            .relative()
            .w_full()
            .h(px(HERO_H))
            .flex_none()
            .overflow_hidden()
            .child(div().absolute().inset_0().child(remote_image(art)))
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
                            .children(meta.into_iter().map(|m| div().child(m)))
                            .children(hero.genres.iter().take(3).map(|g| {
                                div()
                                    .px(px(10.))
                                    .py(px(3.))
                                    .rounded(px(6.))
                                    .border_1()
                                    .border_color(gpui::white().opacity(0.18))
                                    .child(g.clone())
                            })),
                    )
                    .children(hero.synopsis.as_ref().map(|s| {
                        div()
                            .max_w(px(560.))
                            .text_size(px(theme::BODY))
                            .line_height(relative(1.5))
                            .text_color(theme::text_dim())
                            .line_clamp(3)
                            .child(view_models::clamp_words(
                                &view_models::blurb(s),
                                HERO_SYNOPSIS_CHARS,
                            ))
                    }))
                    .child(
                        div()
                            .flex()
                            .gap(px(10.))
                            .pt(px(6.))
                            .child(
                                button(
                                    "hero-play",
                                    busy.clone().unwrap_or(hero.play_label.clone().into()),
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
                                    .on_click(
                                        cx.listener(|page, _, _, cx| page.open_hero_details(cx)),
                                    ),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn media_row(
        &self,
        id: &'static str,
        title: &'static str,
        items: &[AniListMedia],
        scroll: &ScrollHandle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cards = items.iter().enumerate().map(|(i, media)| {
            let route = Route::Show(ShowRef {
                allanime: None,
                anilist_id: Some(media.id),
                title: media_title(media).to_string(),
            });
            let card = poster_card((id, i), media_title(media).to_string(), &media.cover_url)
                .on_click(cx.listener(move |page, _, _, cx| page.navigate(route.clone(), cx)));
            match media.year {
                Some(year) => card.subtitle(year.to_string()),
                None => card,
            }
            .into_any_element()
        });
        row(id, title, scroll, cards).into_any_element()
    }

    fn continue_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let cards = self
            .data
            .continue_items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let label = if item.start > 0.0 {
                    format!("Episode {}", item.ep)
                } else {
                    format!("Episode {} up next", item.ep)
                };
                let art = item.backdrop_url.as_deref().unwrap_or(&item.cover_url);
                let target = item.clone();
                episode_card(
                    ("continue", i),
                    item.title.clone(),
                    label,
                    art,
                    item.fraction,
                )
                .on_click(cx.listener(move |page, _, _, cx| page.play_item(target.clone(), cx)))
                .into_any_element()
            });
        row("continue", "Continue watching", &self.rows[0], cards).into_any_element()
    }
}

fn skeleton() -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(40.))
        .child(div().w_full().h(px(HERO_H)).child(shimmer()))
        .child(
            div()
                .flex()
                .gap(px(18.))
                .px(theme::gutter())
                .children((0..6).map(|_| {
                    div()
                        .flex_none()
                        .w(px(theme::POSTER_W))
                        .h(px(theme::POSTER_H))
                        .rounded(px(10.))
                        .overflow_hidden()
                        .child(shimmer())
                })),
        )
        .into_any_element()
}

impl Render for HomePage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            let hero = self.hero.clone().map(|hero| self.render_hero(&hero, cx));
            let mut rows: Vec<AnyElement> = Vec::new();
            if !self.data.continue_items.is_empty() {
                rows.push(self.continue_row(cx));
            }
            if !self.data.for_you.is_empty() {
                rows.push(self.media_row(
                    "for-you",
                    "For you",
                    &self.data.for_you,
                    &self.rows[1],
                    cx,
                ));
            }
            if !self.data.trending.is_empty() {
                rows.push(self.media_row(
                    "trending",
                    "Trending now",
                    &self.data.trending,
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
        page
    }
}

/// AllAnime and AniList lookups for Continue Watching, with episode lists
/// kept for `EPISODES_TTL`.
struct LiveCatalog<'a> {
    db: &'a Mutex<Db>,
}

/// (show id, mode) → when fetched, episodes.
type EpisodeMemo = HashMap<(String, Mode), (Instant, Vec<String>)>;

static EPISODES: LazyLock<Mutex<EpisodeMemo>> = LazyLock::new(Default::default);

impl Catalog for LiveCatalog<'_> {
    fn search(&self, title: &str, mode: Mode) -> Result<Vec<ShowSummary>> {
        allanime::search(title, mode)
    }

    fn episodes(&self, show: &ShowSummary, mode: Mode) -> Vec<String> {
        let key = (show.id.clone(), mode);
        let memo = || EPISODES.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((at, eps)) = memo().get(&key)
            && at.elapsed() < EPISODES_TTL
        {
            return eps.clone();
        }
        match allanime::episodes(&show.id, mode) {
            Ok(eps) => {
                memo().insert(key, (Instant::now(), eps.clone()));
                eps
            }
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

fn lock(db: &Mutex<Db>) -> std::sync::MutexGuard<'_, Db> {
    db.lock().unwrap_or_else(PoisonError::into_inner)
}

fn settings_quality(cx: &App) -> String {
    cx.try_global::<AppState>().map_or_else(
        || Settings::default().quality,
        |s| s.settings.quality.clone(),
    )
}

fn read_snapshot(db: &Mutex<Db>) -> Option<HomeData> {
    let json = lock(db).cache_get_stale(SNAPSHOT_KEY).ok().flatten()?;
    serde_json::from_str(&json)
        .inspect_err(|err| eprintln!("ani-gui: cached home is unreadable: {err}"))
        .ok()
}

/// Everything Home shows, fetched fresh (catalog calls use their own cache),
/// and the hero's show ready to play.
fn load(
    db: &Mutex<Db>,
    mode: Mode,
    misses: &Mutex<HashSet<(Mode, String)>>,
) -> (HomeData, Option<HeroPlay>) {
    let live = LiveCatalog { db };
    let history = history::read(&paths::ani_cli_history_path());
    let continue_items = {
        let mut misses = misses.lock().unwrap_or_else(PoisonError::into_inner);
        view_models::continue_items(db, &history, mode, &live, &mut misses)
    };
    let trending = catalog::trending(db);
    let seeds: Vec<i64> = continue_items
        .iter()
        .filter_map(|i| i.media.as_ref().map(|m| m.id))
        .take(FOR_YOU_SEEDS)
        .collect();
    let for_you = catalog::for_you(db, &seeds);
    let data = HomeData {
        continue_items,
        trending,
        for_you,
    };
    if !data.is_empty() {
        let stored = serde_json::to_string(&data)
            .map_err(anyhow::Error::from)
            .and_then(|json| lock(db).cache_put(SNAPSHOT_KEY, &json));
        if let Err(err) = stored {
            eprintln!("ani-gui: caching home: {err:#}");
        }
    }
    let hero_play = view_models::hero_pick(&data.continue_items, &data.trending).and_then(|hero| {
        hero_play(db, &live, &hero, mode)
            .inspect_err(|err| eprintln!("ani-gui: preparing {:?}: {err:#}", hero.title))
            .ok()
    });
    (data, hero_play)
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
            let show = catalog::resolve_allanime(media, Mode::Sub)?
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
