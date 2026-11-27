//! Search: a ready-made listing (a preset) before anything is typed, title
//! search, and AniList-style filters (genres and tags to include or leave
//! out, format, status, season and year, source, country, score, length,
//! sort).
//!
//! A plain title search goes to AllAnime, which knows what can actually be
//! streamed. Presets and any filter go to AniList's catalogue instead; the
//! Show page then finds the AllAnime entry for whatever is opened.

use std::collections::HashMap;
use std::time::Duration;

use gpui::{
    AnyElement, Context, Entity, FocusHandle, Focusable, FontWeight, IntoElement, KeyDownEvent,
    ScrollHandle, SharedString, Subscription, Task, WeakEntity, Window, div, prelude::*, px,
    relative,
};

use crate::anilist::{self, Browse};
use crate::app::{AppState, Root, Route, ShowRef};
use crate::db;
use crate::model::{AniListMedia, BrowsePreset, Mode, ShowSummary, display_generation};
use crate::ui::calendar::{civil_from_days, local_day, local_offset};
use crate::ui::components::button::{ButtonKind, button, segmented};
use crate::ui::components::chip::{ChipState, filter_pill, icon_chip};
use crate::ui::components::dropdown::{Dropdown, TriState, dropdown};
use crate::ui::components::poster::poster_card;
use crate::ui::components::text_input::{Changed, TextInput};
use crate::ui::metadata::CardMeta;
use crate::ui::theme;
use crate::ui::view_models::media_title;
use crate::{allanime, catalog};

const DEBOUNCE: Duration = Duration::from_millis(300);
/// Filter clicks wait a moment, so several quick ones make one request.
const FILTER_DEBOUNCE: Duration = Duration::from_millis(150);
const GRID_GAP_X: f32 = 22.;
const GRID_ROW_H: f32 = 308.;
const GRID_ROW_GAP: f32 = 30.;
const GRID_ROW_STRIDE: f32 = GRID_ROW_H + GRID_ROW_GAP;
/// Fallback y-offset of the results grid before its first layout.
const GRID_TOP_Y: f32 = 260.;
/// Oldest year offered in the Year dropdown.
const MIN_YEAR: i32 = 1970;
/// Lowest average scores offered, 0–100.
const MIN_SCORES: [u8; 4] = [60, 70, 80, 90];
/// (menu label, short summary, min, max) episode counts.
const LENGTHS: [(&str, &str, Option<u32>, Option<u32>); 3] = [
    ("Short (1–13)", "Short", None, Some(13)),
    ("Standard (14–26)", "Standard", Some(14), Some(26)),
    ("Long (27+)", "Long", Some(27), None),
];

/// A genre or tag to list shows by, e.g. from a show page's chips.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchFilter {
    Genre(String),
    Tag(String),
}

/// Genres and tags are both include/exclude lists.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Facet {
    Genre,
    Tag,
}

/// Which filter dropdown popover is currently open.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FilterMenu {
    Genres,
    Tags,
    Format,
    Status,
    Season,
    Year,
    Source,
    Country,
    Score,
    Length,
    Sort,
}

/// "WINTER" etc. and year of the anime season containing `now` (December
/// belongs to the next year's winter, as on AniList).
fn current_season(now: i64) -> (&'static str, i32) {
    let (year, month, _) = civil_from_days(local_day(now, local_offset(now)));
    let year = year as i32;
    match month {
        12 => ("WINTER", year + 1),
        1 | 2 => ("WINTER", year),
        3..=5 => ("SPRING", year),
        6..=8 => ("SUMMER", year),
        _ => ("FALL", year),
    }
}

/// Years offered in the Year dropdown: next calendar year down to `MIN_YEAR`.
fn year_options(now: i64) -> Vec<i32> {
    let (year, _, _) = civil_from_days(local_day(now, local_offset(now)));
    let max_year = (year as i32 + 1).max(MIN_YEAR);
    (MIN_YEAR..=max_year).rev().collect()
}

/// The filters a preset stands for, at `now`.
fn preset_filters(preset: BrowsePreset, now: i64) -> Browse {
    let sorted = |sort: &str| Browse {
        sort: sort.into(),
        ..Browse::default()
    };
    match preset {
        BrowsePreset::Trending => sorted("TRENDING_DESC"),
        BrowsePreset::Season => {
            let (season, year) = current_season(now);
            Browse {
                season: Some(season.into()),
                year: Some(year),
                ..sorted("POPULARITY_DESC")
            }
        }
        BrowsePreset::Upcoming => Browse {
            status: Some("NOT_YET_RELEASED".into()),
            ..sorted("POPULARITY_DESC")
        },
        BrowsePreset::AllTime => sorted("POPULARITY_DESC"),
        BrowsePreset::TopRated => sorted("SCORE_DESC"),
    }
}

fn preset_icon(preset: BrowsePreset) -> &'static str {
    match preset {
        BrowsePreset::Trending => "flame",
        BrowsePreset::Season => "calendar",
        BrowsePreset::Upcoming => "sparkles",
        BrowsePreset::AllTime => "heart",
        BrowsePreset::TopRated => "trophy",
    }
}

/// How many filters narrow the listing (the sort order doesn't count).
fn active_filters(b: &Browse) -> usize {
    b.genres.len()
        + b.genres_not.len()
        + b.tags.len()
        + b.tags_not.len()
        + b.formats.len()
        + [
            b.status.is_some(),
            b.season.is_some(),
            b.year.is_some(),
            b.source.is_some(),
            b.country.is_some(),
            b.min_score.is_some(),
            b.min_episodes.is_some() || b.max_episodes.is_some(),
        ]
        .into_iter()
        .filter(|&on| on)
        .count()
}

/// Cycles `name` through include → exclude → neither.
fn cycle(include: &mut Vec<String>, exclude: &mut Vec<String>, name: &str) {
    if let Some(i) = include.iter().position(|n| n == name) {
        include.remove(i);
        exclude.push(name.to_string());
    } else if let Some(i) = exclude.iter().position(|n| n == name) {
        exclude.remove(i);
    } else {
        include.push(name.to_string());
    }
}

/// Toggles `name` in `include`, removing it from `exclude` if it was there.
fn toggle_include(include: &mut Vec<String>, exclude: &mut Vec<String>, name: &str) {
    if let Some(i) = include.iter().position(|n| n == name) {
        include.remove(i);
    } else {
        exclude.retain(|n| n != name);
        include.push(name.to_string());
    }
}

/// Toggles `name` in `exclude`, removing it from `include` if it was there.
fn toggle_exclude(include: &mut Vec<String>, exclude: &mut Vec<String>, name: &str) {
    if let Some(i) = exclude.iter().position(|n| n == name) {
        exclude.remove(i);
    } else {
        include.retain(|n| n != name);
        exclude.push(name.to_string());
    }
}

/// Clears `name` from both include and exclude lists.
fn remove_facet(include: &mut Vec<String>, exclude: &mut Vec<String>, name: &str) {
    include.retain(|n| n != name);
    exclude.retain(|n| n != name);
}

fn facet_state(include: &[String], exclude: &[String], name: &str) -> TriState {
    if include.iter().any(|n| n == name) {
        TriState::Included
    } else if exclude.iter().any(|n| n == name) {
        TriState::Excluded
    } else {
        TriState::Off
    }
}

fn toggle_format(formats: &mut Vec<String>, value: &str) {
    if let Some(i) = formats.iter().position(|v| v == value) {
        formats.remove(i);
    } else {
        formats.push(value.to_string());
    }
}

/// Compact summary for a genre or tag dropdown (e.g. `"Action"`, `"Action +1"`, `"−Gore"`).
fn facet_summary(include: &[String], exclude: &[String]) -> Option<String> {
    let total = include.len() + exclude.len();
    if total == 0 {
        return None;
    }
    let first = if let Some(name) = include.first() {
        name.clone()
    } else {
        format!("\u{2212}{}", exclude[0])
    };
    if total == 1 {
        Some(first)
    } else {
        Some(format!("{first} +{}", total - 1))
    }
}

/// Compact summary for selected formats (e.g. `"TV"`, `"TV, Movie"`, `"TV, Movie +1"`).
fn formats_summary(formats: &[String]) -> Option<String> {
    if formats.is_empty() {
        return None;
    }
    let labels: Vec<&str> = formats
        .iter()
        .map(|v| {
            anilist::FORMATS
                .iter()
                .find(|(code, _)| code == v)
                .map_or(v.as_str(), |(_, label)| *label)
        })
        .collect();
    match labels.as_slice() {
        [only] => Some((*only).to_string()),
        [first, second] => Some(format!("{first}, {second}")),
        [first, second, rest @ ..] => Some(format!("{first}, {second} +{}", rest.len())),
        [] => None,
    }
}

fn option_label(
    options: &'static [(&'static str, &'static str)],
    value: Option<&str>,
) -> Option<&'static str> {
    let value = value?;
    options
        .iter()
        .find(|(v, _)| *v == value)
        .map(|(_, label)| *label)
}

fn score_summary(min_score: Option<u8>) -> Option<String> {
    min_score.map(|s| format!("{}+", f32::from(s) / 10.))
}

fn length_summary(min: Option<u32>, max: Option<u32>) -> Option<&'static str> {
    LENGTHS
        .iter()
        .find(|(_, _, lo, hi)| *lo == min && *hi == max)
        .map(|(_, short, _, _)| *short)
}

fn sort_summary(sort: &str) -> &'static str {
    anilist::SORTS
        .iter()
        .find(|(v, _)| *v == sort)
        .map_or("Popularity", |(_, label)| *label)
}

struct ResultCard {
    title: SharedString,
    cover_url: String,
    meta: CardMeta,
    subtitle: SharedString,
    open: ShowRef,
}

enum State {
    Searching,
    Done,
    Failed(String),
}

/// Where the results came from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    /// Title search: streamable shows.
    AllAnime,
    /// A preset or filtered listing.
    AniList,
}

pub struct SearchPage {
    root: WeakEntity<Root>,
    input: Entity<TextInput>,
    mode: Mode,
    filters: Browse,
    open_menu: Option<FilterMenu>,
    menu_focus: FocusHandle,
    source: Source,
    /// The query `results` are for.
    query: String,
    /// AllAnime title-search results.
    results: Vec<ShowSummary>,
    /// AniList card data for `results` that carry an AniList id, fetched
    /// after them so results appear without waiting on AniList.
    media: HashMap<i64, AniListMedia>,
    /// AniList listing, all pages fetched so far.
    listing: Vec<AniListMedia>,
    page: u32,
    has_more: bool,
    loading_more: Option<Task<()>>,
    /// Precomputed display cards for whichever results are showing.
    cards: Vec<ResultCard>,
    /// Title-language generation `cards` were built with.
    display_gen: u64,
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
        let (mode, preset) = cx
            .try_global::<AppState>()
            .map_or((Mode::default(), BrowsePreset::Trending), |s| {
                (s.settings.mode, s.settings.browse_preset)
            });
        let mut page = SearchPage {
            root,
            input,
            mode,
            filters: preset_filters(preset, db::now()),
            open_menu: None,
            menu_focus: cx.focus_handle(),
            source: Source::AniList,
            query: String::new(),
            results: Vec::new(),
            media: HashMap::new(),
            listing: Vec::new(),
            page: 1,
            has_more: false,
            loading_more: None,
            cards: Vec::new(),
            display_gen: display_generation(),
            state: State::Searching,
            pending: None,
            enrich: None,
            focus_next_render: true,
            scroll: ScrollHandle::new(),
            _changes: changes,
        };
        page.schedule(Duration::ZERO, cx);
        page
    }

    pub fn focus_on_next_render(&mut self) {
        self.focus_next_render = true;
    }

    /// Lists every show with `filter`, clearing the title and other filters.
    pub fn show_only(&mut self, filter: SearchFilter, cx: &mut Context<Self>) {
        self.input
            .update(cx, |input, cx| input.set_text(String::new(), cx));
        let mut filters = Browse::default();
        match filter {
            SearchFilter::Genre(genre) => filters.genres.push(genre),
            SearchFilter::Tag(tag) => filters.tags.push(tag),
        }
        self.filters = filters;
        self.open_menu = None;
        self.focus_next_render = false;
        self.schedule(Duration::ZERO, cx);
    }

    fn open_menu(&mut self, menu: FilterMenu, window: &mut Window, cx: &mut Context<Self>) {
        self.open_menu = Some(menu);
        window.focus(&self.menu_focus);
        cx.notify();
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.open_menu.take().is_some() {
            cx.notify();
        }
    }

    fn rebuild_cards(&mut self) {
        self.display_gen = display_generation();
        let now = db::now();
        let tz = local_offset(now);
        self.cards = match self.source {
            Source::AllAnime => {
                let mode = self.mode;
                self.results
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
                        let title = show.display_title().to_string();
                        ResultCard {
                            title: title.clone().into(),
                            cover_url: show.cover_url.clone(),
                            meta,
                            subtitle,
                            open: ShowRef {
                                anilist_id: show.anilist_id,
                                title,
                                allanime: Some(show.clone()),
                            },
                        }
                    })
                    .collect()
            }
            Source::AniList => self
                .listing
                .iter()
                .map(|media| {
                    let meta = CardMeta::from_media(media, now, tz);
                    let subtitle = meta
                        .facts
                        .iter()
                        .take(2)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" · ");
                    let title = media_title(media).to_string();
                    ResultCard {
                        title: title.clone().into(),
                        cover_url: media.cover_url.clone(),
                        meta,
                        subtitle: subtitle.into(),
                        open: ShowRef {
                            anilist_id: Some(media.id),
                            title,
                            allanime: None,
                        },
                    }
                })
                .collect(),
        };
    }

    fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if self.mode != mode {
            self.mode = mode;
            self.schedule(Duration::ZERO, cx);
        }
    }

    /// Changes the filters and refreshes the results shortly after.
    fn update_filters(&mut self, change: impl FnOnce(&mut Browse), cx: &mut Context<Self>) {
        change(&mut self.filters);
        self.schedule(FILTER_DEBOUNCE, cx);
        cx.notify();
    }

    fn apply_preset(&mut self, preset: BrowsePreset, cx: &mut Context<Self>) {
        self.input
            .update(cx, |input, cx| input.set_text(String::new(), cx));
        self.filters = preset_filters(preset, db::now());
        self.open_menu = None;
        self.schedule(Duration::ZERO, cx);
    }

    fn clear_filters(&mut self, cx: &mut Context<Self>) {
        let sort = self.filters.sort.clone();
        self.open_menu = None;
        self.update_filters(
            |b| {
                *b = Browse {
                    sort,
                    ..Browse::default()
                }
            },
            cx,
        );
    }

    /// The preset the current filters match, if any.
    fn current_preset(&self) -> Option<BrowsePreset> {
        if !self.input_text_is_empty() {
            return None;
        }
        let now = db::now();
        BrowsePreset::ALL
            .iter()
            .copied()
            .find(|&p| preset_filters(p, now) == self.filters)
    }

    fn input_text_is_empty(&self) -> bool {
        self.query.trim().is_empty()
    }

    /// Searches (or lists) after `delay`, replacing any search still
    /// waiting or running.
    fn schedule(&mut self, delay: Duration, cx: &mut Context<Self>) {
        let query = self.input.read(cx).text().trim().to_string();
        let mode = self.mode;
        let source = if !query.is_empty() && active_filters(&self.filters) == 0 {
            Source::AllAnime
        } else {
            Source::AniList
        };
        let mut filters = self.filters.clone();
        filters.search = (!query.is_empty()).then(|| query.clone());
        let db = cx.try_global::<AppState>().map(|s| s.db.clone());
        self.loading_more = None;
        self.pending = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |page, cx| {
                page.state = State::Searching;
                cx.notify();
            })
            .ok();
            let found = cx
                .background_spawn(async move {
                    match source {
                        Source::AllAnime => {
                            allanime::search(&query, mode).map(|r| (query, r, Vec::new(), false))
                        }
                        Source::AniList => {
                            let listed = match &db {
                                Some(db) => catalog::browse(db, &filters, 1),
                                None => anilist::browse(&filters, 1),
                            };
                            listed.map(|(media, more)| (query, Vec::new(), media, more))
                        }
                    }
                })
                .await;
            this.update(cx, |page, cx| {
                page.pending = None;
                page.source = source;
                match found {
                    Ok((query, results, listing, more)) => {
                        page.query = query;
                        page.results = results;
                        page.listing = listing;
                        page.page = 1;
                        page.has_more = more;
                        page.rebuild_cards();
                        page.state = State::Done;
                        page.enrich(cx);
                    }
                    Err(err) => {
                        page.results.clear();
                        page.listing.clear();
                        page.cards.clear();
                        page.has_more = false;
                        page.state = State::Failed(format!("{err:#}"));
                    }
                }
                page.scroll.set_offset(gpui::point(px(0.), px(0.)));
                cx.notify();
            })
            .ok();
        }));
    }

    /// Appends the next page of an AniList listing.
    fn load_more(&mut self, cx: &mut Context<Self>) {
        if self.source != Source::AniList || !self.has_more || self.loading_more.is_some() {
            return;
        }
        let mut filters = self.filters.clone();
        filters.search = (!self.query.is_empty()).then(|| self.query.clone());
        let next = self.page + 1;
        let db = cx.try_global::<AppState>().map(|s| s.db.clone());
        self.loading_more = Some(cx.spawn(async move |this, cx| {
            let found = cx
                .background_spawn(async move {
                    match &db {
                        Some(db) => catalog::browse(db, &filters, next),
                        None => anilist::browse(&filters, next),
                    }
                })
                .await;
            this.update(cx, |page, cx| {
                page.loading_more = None;
                match found {
                    Ok((media, more)) => {
                        let seen: std::collections::HashSet<i64> =
                            page.listing.iter().map(|m| m.id).collect();
                        page.listing
                            .extend(media.into_iter().filter(|m| !seen.contains(&m.id)));
                        page.page = next;
                        page.has_more = more;
                        page.rebuild_cards();
                    }
                    Err(err) => {
                        crate::ui::components::toast::error(
                            cx,
                            format!("Couldn't load more shows: {err:#}"),
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Fetches AniList card data for title-search results in the background.
    fn enrich(&mut self, cx: &mut Context<Self>) {
        if self.source != Source::AllAnime {
            return;
        }
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
        let Some(card) = self.cards.get(index) else {
            return;
        };
        let route = Route::Show(card.open.clone());
        self.root
            .update(cx, |root, cx| root.navigate(route, cx))
            .ok();
    }

    fn status_line(&self) -> Option<String> {
        match &self.state {
            State::Searching if self.cards.is_empty() => Some("Searching…".into()),
            State::Searching => None,
            State::Failed(err) => Some(format!("Search failed: {err}")),
            State::Done if !self.cards.is_empty() => None,
            State::Done => Some(match self.source {
                Source::AllAnime => format!(
                    "No {} results for \u{201c}{}\u{201d}. Try the other title, or switch to {}.",
                    self.mode,
                    self.query,
                    match self.mode {
                        Mode::Sub => "Dub",
                        Mode::Dub => "Sub",
                    }
                ),
                Source::AniList => "No shows match these filters. Try removing one.".into(),
            }),
        }
    }

    /// One line about what is listed.
    fn caption(&self) -> Option<String> {
        if !matches!(self.state, State::Done) || self.cards.is_empty() {
            return None;
        }
        match self.source {
            Source::AllAnime => Some(format!(
                "Title matches with {} episodes. Add a filter to search AniList's catalogue instead.",
                self.mode
            )),
            Source::AniList => match self.current_preset() {
                Some(preset) => Some(format!("{} on AniList", preset.label())),
                None => {
                    let sort = anilist::SORTS
                        .iter()
                        .find(|(v, _)| *v == self.filters.sort)
                        .map_or("popularity", |(_, l)| l);
                    Some(format!("From AniList, by {}", sort.to_lowercase()))
                }
            },
        }
    }

    fn render_presets(&self, cx: &mut Context<Self>) -> AnyElement {
        let current = self.current_preset();
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.))
            .children(BrowsePreset::ALL.iter().map(|&preset| {
                let state = if current == Some(preset) {
                    ChipState::On
                } else {
                    ChipState::Off
                };
                icon_chip(
                    ("search-preset", preset as usize),
                    preset_icon(preset),
                    preset.label(),
                    state,
                )
                .on_click(cx.listener(move |page, _, _, cx| page.apply_preset(preset, cx)))
            }))
            .into_any_element()
    }

    fn menu(
        &self,
        id: &'static str,
        which: FilterMenu,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> Dropdown {
        let view = cx.entity().downgrade();
        dropdown(id, label)
            .open(self.open_menu == Some(which))
            .focus_handle(self.menu_focus.clone())
            .on_open(cx.listener(move |page, _, window, cx| page.open_menu(which, window, cx)))
            .on_close(move |_, cx| {
                view.update(cx, |page, cx| page.close_menu(cx)).ok();
            })
    }

    fn add_facet_items(
        &self,
        mut dd: Dropdown,
        facet: Facet,
        names: &'static [&'static str],
        cx: &mut Context<Self>,
    ) -> Dropdown {
        let (include, exclude) = match facet {
            Facet::Genre => (&self.filters.genres, &self.filters.genres_not),
            Facet::Tag => (&self.filters.tags, &self.filters.tags_not),
        };
        for &name in names {
            let state = facet_state(include, exclude, name);
            dd = dd.tri(
                name,
                state,
                cx.listener(move |page, _, _, cx| {
                    page.update_filters(
                        |b| match facet {
                            Facet::Genre => cycle(&mut b.genres, &mut b.genres_not, name),
                            Facet::Tag => cycle(&mut b.tags, &mut b.tags_not, name),
                        },
                        cx,
                    )
                }),
                cx.listener(move |page, _, _, cx| {
                    page.update_filters(
                        |b| match facet {
                            Facet::Genre => toggle_include(&mut b.genres, &mut b.genres_not, name),
                            Facet::Tag => toggle_include(&mut b.tags, &mut b.tags_not, name),
                        },
                        cx,
                    )
                }),
                cx.listener(move |page, _, _, cx| {
                    page.update_filters(
                        |b| match facet {
                            Facet::Genre => toggle_exclude(&mut b.genres, &mut b.genres_not, name),
                            Facet::Tag => toggle_exclude(&mut b.tags, &mut b.tags_not, name),
                        },
                        cx,
                    )
                }),
            );
        }
        dd
    }

    #[allow(clippy::too_many_arguments)]
    fn single_dropdown(
        &self,
        id: &'static str,
        which: FilterMenu,
        label: &'static str,
        width: f32,
        options: &'static [(&'static str, &'static str)],
        current: Option<&str>,
        pick: fn(&mut Browse) -> &mut Option<String>,
        cx: &mut Context<Self>,
    ) -> Dropdown {
        let mut dd = self
            .menu(id, which, label, cx)
            .menu_width(width)
            .summary(option_label(options, current))
            .active(current.is_some())
            .single(
                "Any",
                current.is_none(),
                cx.listener(move |page, _, _, cx| page.update_filters(|b| *pick(b) = None, cx)),
            );
        for &(value, option_name) in options {
            dd = dd.single(
                option_name,
                current == Some(value),
                cx.listener(move |page, _, _, cx| {
                    page.update_filters(|b| *pick(b) = Some(value.to_string()), cx)
                }),
            );
        }
        dd
    }

    fn render_filter_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let f = &self.filters;
        let count = active_filters(f);

        let genres_summary = facet_summary(&f.genres, &f.genres_not);
        let genres = self.add_facet_items(
            self.menu("search-dd-genres", FilterMenu::Genres, "Genres", cx)
                .menu_width(236.)
                .active(genres_summary.is_some())
                .summary(genres_summary),
            Facet::Genre,
            &anilist::GENRES,
            cx,
        );

        let tags_summary = facet_summary(&f.tags, &f.tags_not);
        let tags = self
            .menu("search-dd-tags", FilterMenu::Tags, "Tags", cx)
            .menu_width(288.)
            .active(tags_summary.is_some())
            .summary(tags_summary)
            .heading("Audience");
        let tags = self
            .add_facet_items(tags, Facet::Tag, &anilist::DEMOGRAPHICS, cx)
            .heading("Themes");
        let tags = self.add_facet_items(tags, Facet::Tag, &anilist::THEME_TAGS, cx);

        let fmt_summary = formats_summary(&f.formats);
        let mut format = self
            .menu("search-dd-format", FilterMenu::Format, "Format", cx)
            .menu_width(184.)
            .active(fmt_summary.is_some())
            .summary(fmt_summary);
        for &(value, label) in &anilist::FORMATS {
            let selected = f.formats.iter().any(|v| v == value);
            format = format.multi(
                label,
                selected,
                cx.listener(move |page, _, _, cx| {
                    page.update_filters(|b| toggle_format(&mut b.formats, value), cx)
                }),
            );
        }

        let status = self.single_dropdown(
            "search-dd-status",
            FilterMenu::Status,
            "Status",
            196.,
            &anilist::STATUSES,
            f.status.as_deref(),
            |b| &mut b.status,
            cx,
        );
        let season = self.single_dropdown(
            "search-dd-season",
            FilterMenu::Season,
            "Season",
            168.,
            &anilist::SEASONS,
            f.season.as_deref(),
            |b| &mut b.season,
            cx,
        );

        let mut year = self
            .menu("search-dd-year", FilterMenu::Year, "Year", cx)
            .menu_width(156.)
            .active(f.year.is_some())
            .summary(f.year.map(|y| y.to_string()))
            .single(
                "Any",
                f.year.is_none(),
                cx.listener(|page, _, _, cx| page.update_filters(|b| b.year = None, cx)),
            );
        for y in year_options(db::now()) {
            year = year.single(
                y.to_string(),
                f.year == Some(y),
                cx.listener(move |page, _, _, cx| page.update_filters(|b| b.year = Some(y), cx)),
            );
        }

        let source = self.single_dropdown(
            "search-dd-source",
            FilterMenu::Source,
            "Source",
            196.,
            &anilist::SOURCES,
            f.source.as_deref(),
            |b| &mut b.source,
            cx,
        );
        let country = self.single_dropdown(
            "search-dd-country",
            FilterMenu::Country,
            "Country",
            184.,
            &anilist::COUNTRIES,
            f.country.as_deref(),
            |b| &mut b.country,
            cx,
        );

        let mut score = self
            .menu("search-dd-score", FilterMenu::Score, "Score", cx)
            .menu_width(156.)
            .active(f.min_score.is_some())
            .summary(score_summary(f.min_score))
            .single(
                "Any",
                f.min_score.is_none(),
                cx.listener(|page, _, _, cx| page.update_filters(|b| b.min_score = None, cx)),
            );
        for min in MIN_SCORES {
            score = score.single(
                format!("{}+", f32::from(min) / 10.),
                f.min_score == Some(min),
                cx.listener(move |page, _, _, cx| {
                    page.update_filters(|b| b.min_score = Some(min), cx)
                }),
            );
        }

        let len_summary = length_summary(f.min_episodes, f.max_episodes);
        let mut length = self
            .menu("search-dd-length", FilterMenu::Length, "Length", cx)
            .menu_width(204.)
            .active(f.min_episodes.is_some() || f.max_episodes.is_some())
            .summary(len_summary)
            .single(
                "Any",
                f.min_episodes.is_none() && f.max_episodes.is_none(),
                cx.listener(|page, _, _, cx| {
                    page.update_filters(
                        |b| {
                            b.min_episodes = None;
                            b.max_episodes = None;
                        },
                        cx,
                    )
                }),
            );
        for &(label, _, min, max) in &LENGTHS {
            let selected = f.min_episodes == min && f.max_episodes == max;
            length = length.single(
                label,
                selected,
                cx.listener(move |page, _, _, cx| {
                    page.update_filters(
                        |b| {
                            b.min_episodes = min;
                            b.max_episodes = max;
                        },
                        cx,
                    )
                }),
            );
        }

        let mut sort = self
            .menu("search-dd-sort", FilterMenu::Sort, "Sort", cx)
            .menu_width(184.)
            .summary(Some(sort_summary(&f.sort)));
        for &(value, label) in &anilist::SORTS {
            sort = sort.single(
                label,
                f.sort == value,
                cx.listener(move |page, _, _, cx| {
                    page.update_filters(|b| b.sort = value.to_string(), cx)
                }),
            );
        }

        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.))
            .child(genres)
            .child(tags)
            .child(format)
            .child(status)
            .child(season)
            .child(year)
            .child(source)
            .child(country)
            .child(score)
            .child(length)
            .child(sort)
            .when(count > 0, |row| {
                row.child(
                    button("search-clear-filters", "Clear filters")
                        .kind(ButtonKind::Ghost)
                        .compact()
                        .on_click(cx.listener(|page, _, _, cx| page.clear_filters(cx))),
                )
            })
            .into_any_element()
    }

    fn render_active_pills(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let f = &self.filters;
        if f.genres.is_empty()
            && f.genres_not.is_empty()
            && f.tags.is_empty()
            && f.tags_not.is_empty()
        {
            return None;
        }
        let mut pills: Vec<AnyElement> = Vec::new();
        for (i, name) in f.genres.iter().enumerate() {
            let remove = name.clone();
            pills.push(
                filter_pill(("pill-genre-inc", i), name.clone(), true)
                    .on_click(cx.listener(move |page, _, _, cx| {
                        let name = remove.clone();
                        page.update_filters(
                            |b| remove_facet(&mut b.genres, &mut b.genres_not, &name),
                            cx,
                        );
                    }))
                    .into_any_element(),
            );
        }
        for (i, name) in f.genres_not.iter().enumerate() {
            let remove = name.clone();
            pills.push(
                filter_pill(("pill-genre-exc", i), name.clone(), false)
                    .on_click(cx.listener(move |page, _, _, cx| {
                        let name = remove.clone();
                        page.update_filters(
                            |b| remove_facet(&mut b.genres, &mut b.genres_not, &name),
                            cx,
                        );
                    }))
                    .into_any_element(),
            );
        }
        for (i, name) in f.tags.iter().enumerate() {
            let remove = name.clone();
            pills.push(
                filter_pill(("pill-tag-inc", i), name.clone(), true)
                    .on_click(cx.listener(move |page, _, _, cx| {
                        let name = remove.clone();
                        page.update_filters(
                            |b| remove_facet(&mut b.tags, &mut b.tags_not, &name),
                            cx,
                        );
                    }))
                    .into_any_element(),
            );
        }
        for (i, name) in f.tags_not.iter().enumerate() {
            let remove = name.clone();
            pills.push(
                filter_pill(("pill-tag-exc", i), name.clone(), false)
                    .on_click(cx.listener(move |page, _, _, cx| {
                        let name = remove.clone();
                        page.update_filters(
                            |b| remove_facet(&mut b.tags, &mut b.tags_not, &name),
                            cx,
                        );
                    }))
                    .into_any_element(),
            );
        }
        Some(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(6.))
                .children(pills)
                .into_any_element(),
        )
    }

    /// The results grid's top, in scrolled-content coordinates, from the
    /// last layout.
    fn grid_top(&self) -> f32 {
        let container = self.scroll.bounds();
        match self.scroll.bounds_for_item(1) {
            // GPUI records child bounds before the scroll offset is applied
            // (see `ScrollHandle::top_item`), so they are already in content
            // coordinates.
            Some(grid) if container.size.height > px(1.) => {
                f32::from(grid.origin.y - container.origin.y)
            }
            _ => GRID_TOP_Y,
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
        let grid_top = self.grid_top();
        let avail = (vw - 2.0 * f32::from(theme::gutter())).max(theme::POSTER_W);
        let cols = ((avail + GRID_GAP_X) / (theme::POSTER_W + GRID_GAP_X))
            .floor()
            .max(1.0) as usize;
        let total_rows = count.div_ceil(cols);
        let scroll_y = (-f32::from(self.scroll.offset().y)).max(0.0);
        let vis_top = (scroll_y - grid_top - GRID_ROW_STRIDE).max(0.0);
        let vis_bottom = (scroll_y + vh - grid_top + GRID_ROW_STRIDE).max(0.0);
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
        if self.display_gen != display_generation() {
            self.rebuild_cards();
        }
        if self.focus_next_render {
            self.focus_next_render = false;
            window.focus(&self.input.focus_handle(cx));
        }
        let mode = self.mode;
        let failed = matches!(self.state, State::Failed(_));
        let searching = matches!(self.state, State::Searching);
        let filter_section = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(self.render_filter_bar(cx))
            .children(self.render_active_pills(cx));
        let header = div()
            .flex()
            .flex_col()
            .gap(px(20.))
            .px(theme::gutter())
            .pt(px(48.))
            .pb(px(28.))
            .child(
                div()
                    .flex()
                    .items_end()
                    .justify_between()
                    .gap(px(16.))
                    .child(
                        div()
                            .font_family(theme::DISPLAY_FONT)
                            .font_weight(FontWeight::BOLD)
                            .text_size(px(theme::PAGE_TITLE))
                            .line_height(px(theme::PAGE_TITLE))
                            .child("Search"),
                    )
                    .children(searching.then(|| {
                        div()
                            .pb(px(6.))
                            .text_size(px(theme::SMALL))
                            .text_color(theme::text_muted())
                            .child("Loading…")
                    })),
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
            .child(self.render_presets(cx))
            .child(filter_section)
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
            .children(self.caption().map(|line| {
                div()
                    .text_size(px(theme::SMALL))
                    .line_height(relative(1.4))
                    .text_color(theme::text_muted())
                    .child(line)
            }));
        let more = (self.source == Source::AniList && self.has_more && !self.cards.is_empty())
            .then(|| {
                div().flex().justify_center().pb(px(64.)).child(
                    button(
                        "search-more",
                        if self.loading_more.is_some() {
                            "Loading…"
                        } else {
                            "Load more"
                        },
                    )
                    .kind(ButtonKind::Secondary)
                    .disabled(self.loading_more.is_some())
                    .on_click(cx.listener(|page, _, _, cx| page.load_more(cx))),
                )
            });
        // Direct children of the scroll container: header, grid, "Load
        // more". `grid_top` reads the grid's bounds as item 1.
        div()
            .id("search")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .capture_key_down(cx.listener(|page, ev: &KeyDownEvent, _, cx| {
                if page.open_menu.is_some() && ev.keystroke.key == "escape" {
                    cx.stop_propagation();
                    page.close_menu(cx);
                }
            }))
            .child(header)
            .child(
                div()
                    .px(theme::gutter())
                    .pb(px(24.))
                    .child(self.render_grid(window, cx)),
            )
            .children(more)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_goes_include_exclude_clear() {
        let (mut inc, mut exc) = (Vec::new(), Vec::new());
        cycle(&mut inc, &mut exc, "Action");
        assert_eq!(
            (inc.as_slice(), exc.len()),
            (["Action".to_string()].as_slice(), 0)
        );
        assert_eq!(facet_state(&inc, &exc, "Action"), TriState::Included);
        cycle(&mut inc, &mut exc, "Action");
        assert!(inc.is_empty());
        assert_eq!(facet_state(&inc, &exc, "Action"), TriState::Excluded);
        cycle(&mut inc, &mut exc, "Action");
        assert!(inc.is_empty() && exc.is_empty());
        assert_eq!(facet_state(&inc, &exc, "Action"), TriState::Off);
    }

    #[test]
    fn toggle_include_exclude_and_remove_facet() {
        let (mut inc, mut exc) = (Vec::new(), Vec::new());
        toggle_include(&mut inc, &mut exc, "Action");
        assert_eq!(facet_state(&inc, &exc, "Action"), TriState::Included);
        // Switching straight to exclude removes from include.
        toggle_exclude(&mut inc, &mut exc, "Action");
        assert_eq!(facet_state(&inc, &exc, "Action"), TriState::Excluded);
        assert!(inc.is_empty());
        // Toggling exclude again clears it.
        toggle_exclude(&mut inc, &mut exc, "Action");
        assert_eq!(facet_state(&inc, &exc, "Action"), TriState::Off);
        // Switching from excluded back to included removes from exclude.
        toggle_exclude(&mut inc, &mut exc, "Gore");
        toggle_include(&mut inc, &mut exc, "Gore");
        assert_eq!(facet_state(&inc, &exc, "Gore"), TriState::Included);
        assert!(exc.is_empty());
        // Explicit removal clears from whichever side holds it.
        remove_facet(&mut inc, &mut exc, "Gore");
        assert!(inc.is_empty() && exc.is_empty());
    }

    #[test]
    fn facet_summary_formats_included_excluded_and_counts() {
        assert_eq!(facet_summary(&[], &[]), None);
        assert_eq!(
            facet_summary(&["Action".into()], &[]),
            Some("Action".into())
        );
        assert_eq!(
            facet_summary(&["Action".into(), "Fantasy".into()], &[]),
            Some("Action +1".into())
        );
        assert_eq!(
            facet_summary(&["Action".into()], &["Gore".into()]),
            Some("Action +1".into())
        );
        assert_eq!(
            facet_summary(&[], &["Gore".into()]),
            Some("\u{2212}Gore".into())
        );
        assert_eq!(
            facet_summary(&[], &["Gore".into(), "Ecchi".into(), "Horror".into()]),
            Some("\u{2212}Gore +2".into())
        );
    }

    #[test]
    fn formats_summary_lists_up_to_two_then_counts() {
        assert_eq!(formats_summary(&[]), None);
        assert_eq!(formats_summary(&["TV".into()]), Some("TV".into()));
        assert_eq!(
            formats_summary(&["TV".into(), "MOVIE".into()]),
            Some("TV, Movie".into())
        );
        assert_eq!(
            formats_summary(&["TV".into(), "MOVIE".into(), "OVA".into()]),
            Some("TV, Movie +1".into())
        );
    }

    #[test]
    fn single_summaries_format_score_length_and_sort() {
        assert_eq!(score_summary(None), None);
        assert_eq!(score_summary(Some(80)), Some("8+".into()));
        assert_eq!(length_summary(None, None), None);
        assert_eq!(length_summary(None, Some(13)), Some("Short"));
        assert_eq!(length_summary(Some(14), Some(26)), Some("Standard"));
        assert_eq!(length_summary(Some(27), None), Some("Long"));
        assert_eq!(sort_summary("SCORE_DESC"), "Score");
        assert_eq!(sort_summary("UNKNOWN"), "Popularity");
    }

    #[test]
    fn year_options_runs_from_next_year_down_to_1970() {
        // 2026-07-01 noon UTC.
        let years = year_options(1_782_907_200);
        assert_eq!(years.first(), Some(&2027));
        assert_eq!(years.last(), Some(&MIN_YEAR));
        assert_eq!(years.len(), (2027 - MIN_YEAR + 1) as usize);
    }

    #[test]
    fn presets_differ_and_only_some_narrow() {
        let now = 1_795_000_000; // November 2026
        let all: Vec<Browse> = BrowsePreset::ALL
            .iter()
            .map(|&p| preset_filters(p, now))
            .collect();
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
        assert_eq!(
            active_filters(&preset_filters(BrowsePreset::Trending, now)),
            0
        );
        assert_eq!(
            active_filters(&preset_filters(BrowsePreset::Season, now)),
            2
        );
        assert_eq!(
            active_filters(&preset_filters(BrowsePreset::Upcoming, now)),
            1
        );
    }

    #[test]
    fn current_season_puts_december_in_next_winter() {
        // 2026-12-15 and 2026-07-01, noon UTC (no zone shifts the month).
        assert_eq!(current_season(1_797_336_000), ("WINTER", 2027));
        assert_eq!(current_season(1_782_907_200), ("SUMMER", 2026));
    }

    #[test]
    fn active_filters_counts_each_narrowing_choice() {
        let b = Browse {
            genres: vec!["Action".into()],
            tags_not: vec!["Gore".into()],
            formats: vec!["TV".into()],
            min_episodes: Some(14),
            max_episodes: Some(26),
            sort: "SCORE_DESC".into(),
            ..Browse::default()
        };
        assert_eq!(active_filters(&b), 4);
    }
}
