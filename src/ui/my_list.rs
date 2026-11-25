//! My List: the shows the user saved, newest first, then what their
//! MyAnimeList/AniList lists hold, grouped by status.

use std::collections::HashSet;
use std::sync::{Mutex, PoisonError};

use anyhow::Result;
use gpui::{
    AnyElement, Context, FontWeight, IntoElement, ScrollHandle, SharedString, Subscription, Task,
    WeakEntity, Window, div, prelude::*, px, relative,
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
const GRID_GAP_X: f32 = 22.;
const GRID_ROW_H: f32 = 308.;
const GRID_ROW_GAP: f32 = 30.;
const GRID_ROW_STRIDE: f32 = GRID_ROW_H + GRID_ROW_GAP;
/// Approximate height of a section heading + gap below it.
const HEADING_STRIDE: f32 = 54.;
/// Vertical gap between sections.
const SECTION_GAP: f32 = 44.;
/// Page top padding + "My List" title + gap before body.
const PAGE_HEADER_H: f32 = 124.;

struct SavedCard {
    title: SharedString,
    cover_url: String,
    meta: CardMeta,
    sub_label: SharedString,
    dub_label: SharedString,
    show_ref: ShowRef,
}

struct ImportedCard {
    anilist_id: i64,
    title: SharedString,
    cover_url: String,
    subtitle: Option<SharedString>,
}

struct ImportedGroup {
    status: ListStatus,
    cards: Vec<ImportedCard>,
}

struct Loaded {
    saved: Vec<SavedCard>,
    groups: Vec<ImportedGroup>,
    count_label: Option<SharedString>,
}

impl Loaded {
    fn is_empty(&self) -> bool {
        self.saved.is_empty() && self.groups.is_empty()
    }
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

    fn open_saved(&self, index: usize, cx: &mut Context<Self>) {
        let State::Ready(loaded) = &self.state else {
            return;
        };
        let Some(card) = loaded.saved.get(index) else {
            return;
        };
        self.navigate(Route::Show(card.show_ref.clone()), cx);
    }

    fn open_imported(&self, status: ListStatus, index: usize, cx: &mut Context<Self>) {
        let State::Ready(loaded) = &self.state else {
            return;
        };
        let Some(card) = loaded
            .groups
            .iter()
            .find(|g| g.status == status)
            .and_then(|g| g.cards.get(index))
        else {
            return;
        };
        self.navigate(
            Route::Show(ShowRef {
                allanime: None,
                anilist_id: Some(card.anilist_id),
                title: card.title.to_string(),
            }),
            cx,
        );
    }

    fn render_saved(
        &self,
        shows: &[SavedCard],
        mode: Mode,
        vp: GridViewport,
        section_top: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        virtual_grid(shows.len(), vp, section_top, |i| {
            let card = &shows[i];
            let subtitle = match mode {
                Mode::Sub => card.sub_label.clone(),
                Mode::Dub => card.dub_label.clone(),
            };
            poster_card(("my-list", i), card.title.clone(), &card.cover_url)
                .meta(card.meta.clone())
                .subtitle(subtitle)
                .on_click(cx.listener(move |page, _, _, cx| page.open_saved(i, cx)))
                .into_any_element()
        })
    }

    /// One status group of imported entries. Cards open the Show page by
    /// AniList id, like the Home and Calendar rows.
    fn render_group(
        &self,
        group: &ImportedGroup,
        vp: GridViewport,
        section_top: f32,
        cx: &mut Context<Self>,
    ) -> (AnyElement, f32) {
        let status = group.status;
        let entries = &group.cards;
        let all = status == ListStatus::Watching || self.expanded.contains(&status);
        let shown = if all {
            entries.len()
        } else {
            entries.len().min(GROUP_PREVIEW)
        };
        let grid_top = section_top + HEADING_STRIDE;
        let grid_el = virtual_grid(shown, vp, grid_top, |i| {
            let entry = &entries[i];
            let card = poster_card((status.as_str(), i), entry.title.clone(), &entry.cover_url)
                .on_click(cx.listener(move |page, _, _, cx| page.open_imported(status, i, cx)));
            match &entry.subtitle {
                Some(subtitle) => card.subtitle(subtitle.clone()),
                None => card,
            }
            .into_any_element()
        });
        let hidden = entries.len() - shown;
        let rows = shown.div_ceil(vp.cols);
        let height = HEADING_STRIDE + rows as f32 * GRID_ROW_STRIDE;
        let el = div()
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
            .child(grid_el)
            .into_any_element();
        (el, height)
    }

    fn render_body(&self, mode: Mode, window: &Window, cx: &mut Context<Self>) -> AnyElement {
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
            State::Ready(loaded) if loaded.is_empty() => {
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
                let (cols, viewport_h) = grid_metrics(&self.scroll, window);
                let vp = GridViewport {
                    cols,
                    scroll_y: (-f32::from(self.scroll.offset().y)).max(0.0),
                    viewport_h,
                };
                let mut cursor_y = PAGE_HEADER_H;
                let mut body = div().flex().flex_col().gap(px(SECTION_GAP));
                if !loaded.saved.is_empty() {
                    let has_heading = !loaded.groups.is_empty();
                    let grid_top = cursor_y + if has_heading { HEADING_STRIDE } else { 0.0 };
                    let saved = self.render_saved(&loaded.saved, mode, vp, grid_top, cx);
                    let rows = loaded.saved.len().div_ceil(vp.cols);
                    let section_h = if has_heading { HEADING_STRIDE } else { 0.0 }
                        + rows as f32 * GRID_ROW_STRIDE;
                    cursor_y += section_h + SECTION_GAP;
                    body = body.child(if !has_heading {
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
                for group in &loaded.groups {
                    let (el, height) = self.render_group(group, vp, cursor_y, cx);
                    cursor_y += height + SECTION_GAP;
                    body = body.child(el);
                }
                body.into_any_element()
            }
        }
    }
}

impl Render for MyListPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = cx
            .try_global::<AppState>()
            .map_or(Mode::default(), |s| s.settings.mode);
        let count = match &self.state {
            State::Ready(l) => l.count_label.clone(),
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
                    .child(self.render_body(mode, window, cx)),
            )
    }
}

#[derive(Clone, Copy)]
struct GridViewport {
    cols: usize,
    scroll_y: f32,
    viewport_h: f32,
}

fn grid_metrics(scroll: &ScrollHandle, window: &Window) -> (usize, f32) {
    let bounds = scroll.bounds();
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
    (cols, vh)
}

fn virtual_grid(
    count: usize,
    vp: GridViewport,
    section_top: f32,
    mut render_card: impl FnMut(usize) -> AnyElement,
) -> AnyElement {
    if count == 0 {
        return div().into_any_element();
    }
    let total_rows = count.div_ceil(vp.cols);
    let total_h = total_rows as f32 * GRID_ROW_STRIDE;
    let vis_top = (vp.scroll_y - section_top - GRID_ROW_STRIDE).max(0.0);
    let vis_bottom = vp.scroll_y + vp.viewport_h - section_top + GRID_ROW_STRIDE;
    if vis_bottom <= 0.0 || vis_top >= total_h {
        return div().h(px(total_h)).into_any_element();
    }
    let start_row = ((vis_top / GRID_ROW_STRIDE).floor() as usize).min(total_rows);
    let end_row = ((vis_bottom / GRID_ROW_STRIDE).ceil().max(0.0) as usize).min(total_rows);
    let top_h = start_row as f32 * GRID_ROW_STRIDE;
    let bottom_h = (total_rows - end_row) as f32 * GRID_ROW_STRIDE;
    let mut container = div().flex().flex_col();
    if top_h > 0.0 {
        container = container.child(div().h(px(top_h)));
    }
    for row in start_row..end_row {
        let first = row * vp.cols;
        let last = (first + vp.cols).min(count);
        container = container.child(
            div()
                .flex()
                .gap(px(GRID_GAP_X))
                .h(px(GRID_ROW_H))
                .mb(px(GRID_ROW_GAP))
                .children((first..last).map(&mut render_card)),
        );
    }
    if bottom_h > 0.0 {
        container = container.child(div().h(px(bottom_h)));
    }
    container.into_any_element()
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

fn build_saved_card(record: ShowRecord, now: i64, tz: i64) -> SavedCard {
    let show = record.summary;
    let title: SharedString = show.display_title().to_string().into();
    let cover_url = record
        .media
        .as_ref()
        .map(|m| m.cover_url.as_str())
        .filter(|c| !c.is_empty())
        .unwrap_or(&show.cover_url)
        .to_string();
    let meta = match &record.media {
        Some(media) => CardMeta::from_media(media, now, tz),
        None => CardMeta::from_summary(&show),
    };
    let sub_label = episodes_label(show.count(Mode::Sub), show.count(Mode::Dub), Mode::Sub).into();
    let dub_label = episodes_label(show.count(Mode::Dub), show.count(Mode::Sub), Mode::Dub).into();
    let show_ref = ShowRef {
        anilist_id: show.anilist_id,
        title: title.to_string(),
        allanime: Some(show),
    };
    SavedCard {
        title,
        cover_url,
        meta,
        sub_label,
        dub_label,
        show_ref,
    }
}

/// The saved shows, newest first (ids without a stored show are skipped),
/// and the imported list entries.
fn load(db: &Mutex<Db>) -> Result<Loaded> {
    let saved_records = {
        let db = db.lock().unwrap_or_else(PoisonError::into_inner);
        db.my_list_shows()?
    };
    let imported = lists::my_list(db)?;
    let now = db::now();
    let tz = local_offset(now);
    let total = saved_records.len() + imported.len();
    let count_label = (total > 0).then(|| match total {
        1 => SharedString::from("1 show"),
        n => SharedString::from(format!("{n} shows")),
    });
    let saved = saved_records
        .into_iter()
        .map(|record| build_saved_card(record, now, tz))
        .collect();
    let grouped = groups(&imported)
        .into_iter()
        .map(|(status, entries)| ImportedGroup {
            status,
            cards: entries
                .into_iter()
                .map(|entry| ImportedCard {
                    anilist_id: entry.anilist_id,
                    title: entry.title.clone().into(),
                    cover_url: entry.cover_url.clone(),
                    subtitle: entry_subtitle(entry).map(Into::into),
                })
                .collect(),
        })
        .collect();
    Ok(Loaded {
        saved,
        groups: grouped,
        count_label,
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
