//! Calendar: what airs over the next seven days, a day at a time.

use std::time::Duration;

use gpui::{
    AnyElement, Context, FontWeight, IntoElement, ScrollHandle, Task, WeakEntity, Window, div,
    prelude::*, px, relative,
};

use crate::app::{AppState, Root, Route, ShowRef};
use crate::model::Airing;
use crate::ui::components::button::{ButtonKind, button};
use crate::ui::components::icon::icon;
use crate::ui::components::image::rounded_image;
use crate::ui::metadata::score_label;
use crate::ui::theme;
use crate::ui::view_models::{format_label, media_title};
use crate::{catalog, db};

const DAY: i64 = 24 * 60 * 60;
const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const DAYS_SHOWN: i64 = 7;
const COVER_W: f32 = 60.;
const COVER_H: f32 = 84.;

const TICK: Duration = Duration::from_secs(60);

/// One airing and its local time, worked out with the UTC offset in force
/// at that moment (so a DST change mid-week doesn't shift it).
struct Slot {
    airing: Airing,
    time: String,
}

/// One day of the week strip.
struct Day {
    weekday: &'static str,
    date: u32,
    slots: Vec<Slot>,
}

impl Day {
    /// The day's airings that haven't aired by `now`.
    fn upcoming(&self, now: i64) -> impl Iterator<Item = &Slot> {
        self.slots.iter().filter(move |s| s.airing.airing_at >= now)
    }
}

enum State {
    Loading,
    Ready(Vec<Day>),
}

pub struct CalendarPage {
    root: WeakEntity<Root>,
    state: State,
    selected: usize,
    load_task: Option<Task<()>>,
    scroll: ScrollHandle,
    /// Repaints every minute, keeping countdowns current and dropping
    /// airings as they pass.
    _tick: Task<()>,
}

impl CalendarPage {
    pub fn new(root: WeakEntity<Root>, cx: &mut Context<Self>) -> Self {
        let mut page = CalendarPage {
            root,
            state: State::Loading,
            selected: 0,
            load_task: None,
            scroll: ScrollHandle::new(),
            _tick: cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(TICK).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }),
        };
        page.reload(cx);
        page
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(db) = cx.try_global::<AppState>().map(|s| s.db.clone()) else {
            return;
        };
        self.state = State::Loading;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let days = cx
                .background_spawn(async move {
                    let mut items = catalog::schedule_week(&db);
                    let now = db::now();
                    // The cached week may have started a while ago.
                    items.retain(|a| a.airing_at >= now);
                    week(&items, now, local_offset(now), local_offset)
                })
                .await;
            this.update(cx, |page, cx| {
                page.load_task = None;
                page.state = State::Ready(days);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        self.selected = index;
        self.scroll.set_offset(gpui::point(px(0.), px(0.)));
        cx.notify();
    }

    fn open(&self, airing: &Airing, cx: &mut Context<Self>) {
        let route = Route::Show(ShowRef {
            allanime: None,
            anilist_id: Some(airing.media.id),
            title: media_title(&airing.media).to_string(),
        });
        self.root
            .update(cx, |root, cx| root.navigate(route, cx))
            .ok();
    }

    fn render_strip(&self, days: &[Day], now: i64, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .gap(px(8.))
            .children(days.iter().enumerate().map(|(i, day)| {
                let selected = i == self.selected;
                let count = match day.upcoming(now).count() {
                    0 => "None".to_string(),
                    1 => "1 episode".to_string(),
                    n => format!("{n} episodes"),
                };
                div()
                    .id(("day", i))
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .px(px(14.))
                    .pt(px(12.))
                    .pb(px(14.))
                    .rounded(px(10.))
                    .overflow_hidden()
                    .child(
                        div()
                            .text_size(px(theme::SMALL))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(if selected {
                                theme::text()
                            } else {
                                theme::text_dim()
                            })
                            .child(if i == 0 { "Today" } else { day.weekday }),
                    )
                    .child(
                        div()
                            .font_family(theme::DISPLAY_FONT)
                            .font_weight(FontWeight::BOLD)
                            .text_size(px(44.))
                            .line_height(px(46.))
                            .text_color(if selected {
                                theme::text()
                            } else {
                                theme::text_muted()
                            })
                            .child(day.date.to_string()),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::text_muted())
                            .truncate()
                            .child(count),
                    )
                    .when(selected, |tile| {
                        tile.bg(theme::raised()).child(
                            div()
                                .absolute()
                                .bottom(px(0.))
                                .left(px(14.))
                                .right(px(14.))
                                .h(px(3.))
                                .rounded_t(px(2.))
                                .bg(theme::accent()),
                        )
                    })
                    .when(!selected, |tile| {
                        tile.cursor_pointer()
                            .hover(|s| s.bg(theme::wash()))
                            .on_click(cx.listener(move |page, _, _, cx| page.select(i, cx)))
                    })
            }))
            .into_any_element()
    }

    fn render_day(&self, days: &[Day], now: i64, cx: &mut Context<Self>) -> AnyElement {
        let Some(day) = days.get(self.selected) else {
            return div().into_any_element();
        };
        let slots: Vec<&Slot> = day.upcoming(now).collect();
        if slots.is_empty() {
            let when = if self.selected == 0 {
                "Nothing else airs today.".to_string()
            } else {
                format!("Nothing new airs on {} {}.", day.weekday, day.date)
            };
            return div()
                .pt(px(8.))
                .text_color(theme::text_dim())
                .child(when)
                .into_any_element();
        }
        // The very next airing of the week gets a countdown.
        let next_at = days
            .iter()
            .find_map(|d| d.upcoming(now).next())
            .map(|s| (s.airing.media.id, s.airing.airing_at));
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .children(slots.into_iter().enumerate().map(|(i, slot)| {
                let airing = &slot.airing;
                let countdown = (next_at == Some((airing.media.id, airing.airing_at)))
                    .then(|| until_label(airing.airing_at - now));
                self.render_airing(i, slot, countdown, cx)
            }))
            .into_any_element()
    }

    fn render_airing(
        &self,
        index: usize,
        slot: &Slot,
        countdown: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let airing = &slot.airing;
        let media = &airing.media;
        let episode = match media.episodes {
            Some(total) => format!("Episode {} of {total}", airing.episode),
            None => format!("Episode {}", airing.episode),
        };
        let target = airing.clone();
        div()
            .id(("airing", index))
            .group("airing")
            .flex()
            .items_center()
            .gap(px(20.))
            .p(px(10.))
            .rounded(px(12.))
            .cursor_pointer()
            .hover(|s| s.bg(theme::raised()))
            .on_click(cx.listener(move |page, _, _, cx| page.open(&target, cx)))
            .child(
                div()
                    .flex_none()
                    .w(px(64.))
                    .font_family(theme::DISPLAY_FONT)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(px(theme::SECTION))
                    .text_color(theme::text())
                    .child(slot.time.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(COVER_W))
                    .h(px(COVER_H))
                    .child(rounded_image(
                        &media.cover_url,
                        px(8.),
                        theme::night(),
                        Some(("airing", theme::raised().into())),
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(media_title(media).to_string()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(px(14.))
                            .text_size(px(theme::SMALL))
                            .text_color(theme::text_dim())
                            .child(episode)
                            .children(media.score.and_then(score_label).map(|score| {
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(4.))
                                    .child(icon("star").size(px(12.)).text_color(theme::accent()))
                                    .child(score)
                            }))
                            .children(media.format.as_deref().map(format_label))
                            .children(
                                media.genres.iter().take(2).map(|g| {
                                    div().text_color(theme::text_muted()).child(g.clone())
                                }),
                            ),
                    ),
            )
            .children(countdown.map(|c| {
                div()
                    .flex_none()
                    .pr(px(8.))
                    .text_size(px(theme::SMALL))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::accent())
                    .child(c)
            }))
            .into_any_element()
    }
}

impl Render for CalendarPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = db::now();
        let body: AnyElement = match &self.state {
            State::Loading => div()
                .text_color(theme::text_muted())
                .child("Loading the schedule…")
                .into_any_element(),
            State::Ready(days) if days.iter().all(|d| d.upcoming(now).next().is_none()) => div()
                .flex()
                .flex_col()
                .items_start()
                .gap(px(14.))
                .child(
                    div()
                        .max_w(px(560.))
                        .line_height(relative(1.5))
                        .text_color(theme::text_dim())
                        .child(
                            "No schedule to show. AniList may be unreachable; \
                             check your connection and try again.",
                        ),
                )
                .child(
                    button("calendar-retry", "Try again")
                        .kind(ButtonKind::Secondary)
                        .compact()
                        .disabled(self.load_task.is_some())
                        .on_click(cx.listener(|page, _, _, cx| page.reload(cx))),
                )
                .into_any_element(),
            State::Ready(days) => div()
                .flex()
                .flex_col()
                .gap(px(24.))
                .child(self.render_strip(days, now, cx))
                .child(self.render_day(days, now, cx))
                .into_any_element(),
        };
        div()
            .id("calendar")
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
                            .flex_col()
                            .gap(px(10.))
                            .child(
                                div()
                                    .font_family(theme::DISPLAY_FONT)
                                    .font_weight(FontWeight::BOLD)
                                    .text_size(px(theme::PAGE_TITLE))
                                    .line_height(px(theme::PAGE_TITLE))
                                    .child("Calendar"),
                            )
                            .child(div().text_color(theme::text_dim()).child(
                                "New episodes over the next seven days, in your local time.",
                            )),
                    )
                    .child(body),
            )
    }
}

/// The seven days from `now`'s, each with its airings from `items`:
/// grouped into days with `tz_offset_secs` (the offset at `now`), each time
/// shown with `offset_at` its own moment.
fn week(
    items: &[Airing],
    now: i64,
    tz_offset_secs: i64,
    offset_at: impl Fn(i64) -> i64,
) -> Vec<Day> {
    let mut groups = group_by_day(items, tz_offset_secs);
    let today = local_day(now, tz_offset_secs);
    (today..today + DAYS_SHOWN)
        .map(|day| {
            let label = day_label(day);
            let slots = groups
                .iter_mut()
                .find(|(l, _)| *l == label)
                .map(|(_, list)| std::mem::take(list))
                .unwrap_or_default()
                .into_iter()
                .map(|airing| Slot {
                    time: time_label(airing.airing_at, offset_at(airing.airing_at)),
                    airing,
                })
                .collect();
            let (weekday, date) = day_parts(day);
            Day {
                weekday,
                date,
                slots,
            }
        })
        .collect()
}

/// "in 45 min", "in 3 h 20 min".
fn until_label(secs: i64) -> String {
    let mins = (secs.max(0) + 59) / 60;
    match (mins / 60, mins % 60) {
        (0, 0) => "Airing now".into(),
        (0, m) => format!("Airs in {m} min"),
        (h, 0) => format!("Airs in {h} h"),
        (h, m) => format!("Airs in {h} h {m} min"),
    }
}

/// Seconds the local time zone is east of UTC at `ts`.
pub fn local_offset(ts: i64) -> i64 {
    let t: nix::libc::time_t = ts;
    // SAFETY: `tm` is plain data that localtime_r fills; both pointers are
    // valid for the call.
    unsafe {
        let mut tm: nix::libc::tm = std::mem::zeroed();
        if nix::libc::localtime_r(&t, &mut tm).is_null() {
            return 0;
        }
        tm.tm_gmtoff
    }
}

/// Days since the epoch, in the zone `tz_offset_secs` east of UTC.
pub fn local_day(ts: i64, tz_offset_secs: i64) -> i64 {
    (ts + tz_offset_secs).div_euclid(DAY)
}

/// Weekday name and day of the month of `day` (days since the epoch).
pub fn day_parts(day: i64) -> (&'static str, u32) {
    // 1970-01-01 was a Thursday.
    let weekday = WEEKDAYS[(day + 4).rem_euclid(7) as usize];
    (weekday, civil_from_days(day).2)
}

/// "Mon 24".
pub fn day_label(day: i64) -> String {
    let (weekday, dom) = day_parts(day);
    format!("{weekday} {dom}")
}

/// Local "HH:MM" of `ts`.
pub fn time_label(ts: i64, tz_offset_secs: i64) -> String {
    let secs = (ts + tz_offset_secs).rem_euclid(DAY);
    format!("{:02}:{:02}", secs / 3600, secs % 3600 / 60)
}

/// `items` by local day, earliest first, each day's airings in time order.
pub fn group_by_day(items: &[Airing], tz_offset_secs: i64) -> Vec<(String, Vec<Airing>)> {
    let mut sorted = items.to_vec();
    sorted.sort_by_key(|a| a.airing_at);
    let mut days: Vec<(i64, Vec<Airing>)> = Vec::new();
    for airing in sorted {
        let day = local_day(airing.airing_at, tz_offset_secs);
        match days.last_mut() {
            Some((d, list)) if *d == day => list.push(airing),
            _ => days.push((day, vec![airing])),
        }
    }
    days.into_iter()
        .map(|(day, list)| (day_label(day), list))
        .collect()
}

/// (year, month, day) of `days` since 1970-01-01, proleptic Gregorian
/// (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AniListMedia;

    fn airing(id: i64, airing_at: i64) -> Airing {
        Airing {
            media: AniListMedia {
                id,
                title_romaji: format!("Show {id}"),
                ..AniListMedia::default()
            },
            episode: 1,
            airing_at,
        }
    }

    const MON_NOON_UTC: i64 = 1_795_435_200; // 2026-11-23 12:00 UTC, a Monday
    const MON_2330_UTC: i64 = 1_795_476_600;
    const TUE_10_UTC: i64 = 1_795_514_400;

    #[test]
    fn group_by_day_orders_and_labels() {
        let items = [
            airing(1, TUE_10_UTC),
            airing(2, MON_2330_UTC),
            airing(3, MON_NOON_UTC),
        ];
        let ids = |groups: &[(String, Vec<Airing>)]| -> Vec<(String, Vec<i64>)> {
            groups
                .iter()
                .map(|(label, day)| (label.clone(), day.iter().map(|a| a.media.id).collect()))
                .collect()
        };
        // UTC: Monday holds the noon and 23:30 airings.
        assert_eq!(
            ids(&group_by_day(&items, 0)),
            vec![
                ("Mon 23".to_string(), vec![3, 2]),
                ("Tue 24".to_string(), vec![1])
            ]
        );
        // An hour east, 23:30 UTC is already Tuesday.
        assert_eq!(
            ids(&group_by_day(&items, 3600)),
            vec![
                ("Mon 23".to_string(), vec![3]),
                ("Tue 24".to_string(), vec![2, 1])
            ]
        );
        // Far west, Monday noon UTC is Monday morning; Tuesday 10:00 UTC is
        // still Monday night.
        assert_eq!(
            ids(&group_by_day(&items, -11 * 3600)),
            vec![("Mon 23".to_string(), vec![3, 2, 1])]
        );
        assert!(group_by_day(&[], 0).is_empty());
    }

    #[test]
    fn week_has_seven_days_from_today() {
        let items = [airing(1, MON_NOON_UTC), airing(2, TUE_10_UTC)];
        let days = week(&items, MON_NOON_UTC - 3600, 0, |_| 0);
        assert_eq!(days.len(), 7);
        assert_eq!((days[0].weekday, days[0].date), ("Mon", 23));
        assert_eq!((days[6].weekday, days[6].date), ("Sun", 29));
        assert_eq!(days[0].slots.len(), 1);
        assert_eq!(days[1].slots[0].airing.media.id, 2);
        assert!(days[2..].iter().all(|d| d.slots.is_empty()));
        // Once an airing's time has passed it is no longer upcoming.
        assert_eq!(days[0].upcoming(MON_NOON_UTC).count(), 1);
        assert_eq!(days[0].upcoming(MON_NOON_UTC + 1).count(), 0);
    }

    #[test]
    fn week_times_use_each_airings_own_offset() {
        let items = [airing(1, MON_NOON_UTC), airing(2, TUE_10_UTC)];
        // Clocks go forward an hour between the two airings.
        let offset_at = |ts: i64| if ts < TUE_10_UTC { 3600 } else { 7200 };
        let days = week(&items, MON_NOON_UTC - 3600, 3600, offset_at);
        assert_eq!(days[0].slots[0].time, "13:00");
        assert_eq!(days[1].slots[0].time, "12:00");
    }

    #[test]
    fn labels_and_times() {
        assert_eq!(day_label(0), "Thu 1");
        assert_eq!(day_label(local_day(MON_NOON_UTC, 0)), "Mon 23");
        // Month and year boundaries.
        assert_eq!(civil_from_days(local_day(1_798_675_200, 0)), (2026, 12, 31));
        assert_eq!(civil_from_days(local_day(1_798_761_600, 0)), (2027, 1, 1));
        assert_eq!(time_label(MON_2330_UTC, 0), "23:30");
        assert_eq!(time_label(MON_2330_UTC, 3600), "00:30");
        assert_eq!(time_label(MON_NOON_UTC, -5 * 3600 - 1800), "06:30");
    }
}
