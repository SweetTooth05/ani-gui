//! Settings: appearance, playback, browsing and download preferences, and a
//! check of what ani-gui needs installed.

use std::path::{Path, PathBuf};
use std::sync::PoisonError;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gpui::{
    Animation, AnimationExt, AnyElement, ClickEvent, Context, Entity, FontWeight, IntoElement,
    ObjectFit, ScrollHandle, SharedString, Subscription, Task, Transformation, Window, div, img,
    percentage, prelude::*, px, relative,
};

use crate::app::AppState;
use crate::diagnostics::{self, Report};
use crate::model::{
    Accent, BrowsePreset, ClockFormat, Mode, SEEK_CHOICES, Settings, StartPage, Theme,
    TitleLanguage, UP_NEXT_CHOICES,
};
use crate::ui::accounts::AccountsSection;
use crate::ui::components::button::{ButtonKind, button, segmented};
use crate::ui::components::chip::{ChipState, icon_chip};
use crate::ui::components::icon::icon;
use crate::ui::components::text_input::{Changed, TextInput};
use crate::ui::components::toast;
use crate::ui::files::{display_path, open_folder};
use crate::ui::show::QUALITIES;
use crate::ui::theme;
use crate::updater::{self, Status, UpdateAction, UpdateNotice};

const CONTENT_W: f32 = 780.;

/// Bumped on every settings save, so an older save finishing late is
/// skipped.
static SAVE_GENERATION: AtomicU64 = AtomicU64::new(0);

pub struct SettingsPage {
    settings: Settings,
    dir_input: Entity<TextInput>,
    dir_error: Option<String>,
    saving_dir: bool,
    report: Option<Report>,
    check_task: Option<Task<()>>,
    scroll: ScrollHandle,
    _dir_changes: Subscription,
    accounts: Entity<AccountsSection>,
}

impl SettingsPage {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let settings = cx
            .try_global::<AppState>()
            .map(|s| s.settings.clone())
            .unwrap_or_default();
        let dir_input = cx.new(|cx| {
            let mut input = TextInput::new("Folder for downloads", cx);
            input.set_text(settings.download_dir.to_string_lossy().into_owned(), cx);
            input
        });
        let dir_changes = cx.subscribe(&dir_input, |page, _, _: &Changed, cx| {
            page.dir_error = None;
            cx.notify();
        });
        let mut page = SettingsPage {
            settings,
            dir_input,
            dir_error: None,
            saving_dir: false,
            report: None,
            check_task: None,
            scroll: ScrollHandle::new(),
            _dir_changes: dir_changes,
            accounts: cx.new(AccountsSection::new),
        };
        page.check(cx);
        if let Some(updates) = updater::model(cx) {
            cx.observe(&updates, |_, _, cx| cx.notify()).detach();
        }
        page
    }

    fn check(&mut self, cx: &mut Context<Self>) {
        if self.check_task.is_some() {
            return;
        }
        self.check_task = Some(cx.spawn(async move |this, cx| {
            let report = cx.background_spawn(async { diagnostics::run() }).await;
            this.update(cx, |page, cx| {
                page.check_task = None;
                page.report = Some(report);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Makes `settings` current for everything opened from now on and
    /// saves it in the background.
    fn apply(&mut self, settings: Settings, cx: &mut Context<Self>) {
        let Some(state) = cx.try_global::<AppState>() else {
            return;
        };
        let db = state.db.clone();
        cx.global_mut::<AppState>().settings = settings.clone();
        let restyled = settings.theme != self.settings.theme
            || settings.accent != self.settings.accent
            || settings.title_language != self.settings.title_language;
        self.settings = settings.clone();
        if restyled {
            crate::app::apply_display_prefs(&settings);
            cx.refresh_windows();
        }
        let mine = SAVE_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
        cx.notify();
        cx.spawn(async move |_, cx| {
            let saved = cx
                .background_spawn(async move {
                    let db = db.lock().unwrap_or_else(PoisonError::into_inner);
                    if SAVE_GENERATION.load(Ordering::SeqCst) != mine {
                        return Ok(());
                    }
                    db.save_settings(&settings)
                })
                .await;
            if let Err(err) = saved {
                cx.update(|cx| toast::error(cx, format!("Couldn't save settings: {err:#}")))
                    .ok();
            }
        })
        .detach();
    }

    /// Applies `change` to a copy of the settings and saves it if anything
    /// changed.
    fn update(&mut self, change: impl FnOnce(&mut Settings), cx: &mut Context<Self>) {
        let mut settings = self.settings.clone();
        change(&mut settings);
        if settings != self.settings {
            self.apply(settings, cx);
        }
    }

    /// A click handler that applies `change`.
    fn on(
        &self,
        change: impl Fn(&mut Settings) + 'static,
        cx: &mut Context<Self>,
    ) -> impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static {
        cx.listener(move |page, _, _, cx| page.update(&change, cx))
    }

    /// The folder typed in, if it differs from the current one.
    fn typed_dir(&self, cx: &Context<Self>) -> Option<String> {
        let typed = self.dir_input.read(cx).text().trim().to_string();
        (Path::new(&typed) != self.settings.download_dir).then_some(typed)
    }

    /// Checks the typed folder (creating it, and writing a file in it),
    /// then makes it the download folder.
    fn save_dir(&mut self, cx: &mut Context<Self>) {
        let Some(typed) = self.typed_dir(cx) else {
            return;
        };
        let dir = match parse_dir(&typed, std::env::home_dir().as_deref()) {
            Ok(dir) => dir,
            Err(err) => {
                self.dir_error = Some(err);
                cx.notify();
                return;
            }
        };
        self.saving_dir = true;
        self.dir_error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let made = {
                let dir = dir.clone();
                cx.background_spawn(async move { prepare_dir(&dir) }).await
            };
            this.update(cx, |page, cx| {
                page.saving_dir = false;
                match made {
                    Ok(()) => {
                        page.dir_input.update(cx, |input, cx| {
                            input.set_text(dir.to_string_lossy().into_owned(), cx)
                        });
                        let settings = Settings {
                            download_dir: dir,
                            ..page.settings.clone()
                        };
                        page.apply(settings, cx);
                    }
                    Err(err) => page.dir_error = Some(format!("Can't use that folder: {err}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_appearance(&self, cx: &mut Context<Self>) -> AnyElement {
        let s = &self.settings;
        let themes = div()
            .flex()
            .flex_wrap()
            .gap(px(12.))
            .children(Theme::ALL.iter().map(|&t| {
                let p = theme::theme_preview(t);
                let selected = s.theme == t;
                let accent_pill: gpui::Hsla = {
                    let mut c: gpui::Hsla = p.accent.into();
                    c.a = if p.light { 0.16 } else { 0.25 };
                    c
                };
                div()
                    .id(("settings-theme", t as usize))
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .cursor_pointer()
                    .on_click(self.on(move |s| s.theme = t, cx))
                    .child(
                        div()
                            .w(px(136.))
                            .h(px(84.))
                            .rounded(px(10.))
                            .overflow_hidden()
                            .bg(p.page)
                            .border_2()
                            .border_color(if selected {
                                theme::accent()
                            } else {
                                theme::control_border()
                            })
                            .when(theme::is_light(), |d| d.shadow(theme::shadow_sm()))
                            .flex()
                            .child(
                                div()
                                    .w(px(30.))
                                    .h_full()
                                    .bg(p.sidebar)
                                    .border_r_1()
                                    .border_color(p.border)
                                    .p(px(6.))
                                    .flex()
                                    .flex_col()
                                    .gap(px(5.))
                                    .child(div().w(px(14.)).h(px(5.)).rounded(px(2.)).bg(p.accent))
                                    .child(
                                        div()
                                            .mt(px(4.))
                                            .w_full()
                                            .h(px(6.))
                                            .rounded(px(2.))
                                            .bg(if p.light { accent_pill } else { p.tile.into() }),
                                    )
                                    .child(div().w(px(14.)).h(px(4.)).rounded(px(2.)).bg(p.tile))
                                    .child(div().w(px(12.)).h(px(4.)).rounded(px(2.)).bg(p.tile)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .h_full()
                                    .p(px(8.))
                                    .flex()
                                    .flex_col()
                                    .gap(px(6.))
                                    .child(
                                        div()
                                            .w_full()
                                            .h(px(34.))
                                            .p(px(6.))
                                            .rounded(px(5.))
                                            .bg(p.card)
                                            .when(p.light, |d| d.border_1().border_color(p.border))
                                            .flex()
                                            .flex_col()
                                            .justify_between()
                                            .child(
                                                div()
                                                    .w(px(48.))
                                                    .h(px(5.))
                                                    .rounded(px(2.))
                                                    .bg(p.tile),
                                            )
                                            .child(
                                                div()
                                                    .w(px(26.))
                                                    .h(px(9.))
                                                    .rounded(px(3.))
                                                    .bg(p.accent),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .flex()
                                            .gap(px(5.))
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .h_full()
                                                    .rounded(px(4.))
                                                    .bg(p.card)
                                                    .when(p.light, |d| {
                                                        d.border_1().border_color(p.border)
                                                    }),
                                            )
                                            .child(
                                                div().flex_1().h_full().rounded(px(4.)).bg(p.tile),
                                            )
                                            .child(
                                                div().flex_1().h_full().rounded(px(4.)).bg(p.tile),
                                            ),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(theme::SMALL))
                            .font_weight(if selected {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::MEDIUM
                            })
                            .text_color(if selected {
                                theme::text()
                            } else {
                                theme::text_dim()
                            })
                            .child(t.label()),
                    )
            }));
        let accents = div()
            .flex()
            .gap(px(10.))
            .children(Accent::ALL.iter().map(|&a| {
                let selected = s.accent == a;
                div()
                    .id(("settings-accent", a as usize))
                    .size(px(34.))
                    .p(px(3.))
                    .rounded_full()
                    .border_2()
                    .border_color(if selected {
                        theme::text()
                    } else {
                        gpui::transparent_black().into()
                    })
                    .cursor_pointer()
                    .on_click(self.on(move |s| s.accent = a, cx))
                    .child(div().size_full().rounded_full().bg(theme::accent_swatch(a)))
            }));
        let accent_name = s.accent.label();
        section("Appearance")
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .py(px(18.))
                    .border_b_1()
                    .border_color(theme::line())
                    .child(label_block(
                        "Theme",
                        "The video player stays dark whichever you pick.",
                    ))
                    .child(themes),
            )
            .child(setting_row(
                "Accent colour",
                format!("{accent_name}. For Play buttons, progress and the current page."),
                accents.into_any_element(),
            ))
            .child(setting_row(
                "Titles",
                "Which name to show when a show has both. Pages you open from now on use it.",
                self.choices(
                    "settings-titles",
                    TitleLanguage::ALL,
                    s.title_language,
                    TitleLanguage::label,
                    |s, v| s.title_language = v,
                    cx,
                ),
            ))
            .child(setting_row(
                "Clock",
                "How airing times are written in the Calendar.",
                self.choices(
                    "settings-clock",
                    ClockFormat::ALL,
                    s.clock,
                    ClockFormat::label,
                    |s, v| s.clock = v,
                    cx,
                ),
            ))
            .child(setting_row(
                "Start page",
                "Where ani-gui opens.",
                self.choices(
                    "settings-start",
                    StartPage::ALL,
                    s.start_page,
                    StartPage::label,
                    |s, v| s.start_page = v,
                    cx,
                ),
            ))
            .child(setting_row(
                "Poster details",
                "Score, genres and a synopsis over a poster while the pointer is on it.",
                switch("settings-poster-details", s.poster_details)
                    .on_click(self.on(|s| s.poster_details = !s.poster_details, cx))
                    .into_any_element(),
            ))
            .into_any_element()
    }

    /// A segmented control over `all`, applying `set` with the pick.
    fn choices<T: Copy + PartialEq + 'static>(
        &self,
        id: &'static str,
        all: &'static [T],
        current: T,
        label: fn(T) -> &'static str,
        set: fn(&mut Settings, T),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut control = segmented(id);
        for &value in all {
            control = control.option(
                label(value),
                current == value,
                self.on(move |s| set(s, value), cx),
            );
        }
        control.into_any_element()
    }

    /// A segmented control over seconds.
    fn seconds(
        &self,
        id: &'static str,
        all: &'static [u32],
        current: u32,
        set: fn(&mut Settings, u32),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut control = segmented(id);
        for &secs in all {
            control = control.option(
                format!("{secs} s"),
                current == secs,
                self.on(move |s| set(s, secs), cx),
            );
        }
        control.into_any_element()
    }

    fn render_playback(&self, cx: &mut Context<Self>) -> AnyElement {
        let s = &self.settings;
        let mut quality = segmented("settings-quality");
        for (value, label) in QUALITIES {
            quality = quality.option(
                label,
                s.quality == value,
                self.on(move |s| s.quality = value.to_string(), cx),
            );
        }
        section("Playback")
            .child(setting_row(
                "Audio",
                "Sub or Dub, for shows you open from now on.",
                self.choices(
                    "settings-mode",
                    &[Mode::Sub, Mode::Dub],
                    s.mode,
                    |m| match m {
                        Mode::Sub => "Sub",
                        Mode::Dub => "Dub",
                    },
                    |s, v| s.mode = v,
                    cx,
                ),
            ))
            .child(setting_row(
                "Quality",
                "For streams and downloads. A show's page can pick another.",
                quality.into_any_element(),
            ))
            .child(setting_row(
                "Autoplay next episode",
                "Starts the next episode after a short countdown when one ends.",
                switch("settings-autoplay", s.autoplay_next)
                    .on_click(self.on(|s| s.autoplay_next = !s.autoplay_next, cx))
                    .into_any_element(),
            ))
            .child(setting_row(
                "Up next countdown",
                "How long the Up next card waits before playing.",
                self.seconds(
                    "settings-up-next",
                    &UP_NEXT_CHOICES,
                    s.up_next_secs,
                    |s, v| s.up_next_secs = v,
                    cx,
                ),
            ))
            .child(setting_row(
                "Skip openings automatically",
                "Jumps past an episode's opening when its timing is known (AniSkip or chapters). Once per episode.",
                switch("settings-auto-skip", s.auto_skip_intro)
                    .on_click(self.on(|s| s.auto_skip_intro = !s.auto_skip_intro, cx))
                    .into_any_element(),
            ))
            .child(setting_row(
                "Skip step",
                "How far the player's skip buttons and arrow keys jump.",
                self.seconds(
                    "settings-seek",
                    &SEEK_CHOICES,
                    s.seek_secs,
                    |s, v| s.seek_secs = v,
                    cx,
                ),
            ))
            .child(setting_row(
                "Keep screen awake",
                "Stops the screen from dimming or locking while a video plays.",
                switch("settings-keep-awake", s.keep_awake)
                    .on_click(self.on(|s| s.keep_awake = !s.keep_awake, cx))
                    .into_any_element(),
            ))
            .into_any_element()
    }

    fn render_browsing(&self, cx: &mut Context<Self>) -> AnyElement {
        let s = &self.settings;
        let presets = div()
            .flex()
            .flex_wrap()
            .justify_end()
            .gap(px(6.))
            .max_w(px(420.))
            .children(BrowsePreset::ALL.iter().map(|&p| {
                icon_chip(
                    ("settings-preset", p as usize),
                    match p {
                        BrowsePreset::Trending => "flame",
                        BrowsePreset::Season => "calendar",
                        BrowsePreset::Upcoming => "sparkles",
                        BrowsePreset::AllTime => "heart",
                        BrowsePreset::TopRated => "trophy",
                    },
                    p.label(),
                    if s.browse_preset == p {
                        ChipState::On
                    } else {
                        ChipState::Off
                    },
                )
                .on_click(self.on(move |s| s.browse_preset = p, cx))
            }));
        section("Browsing")
            .child(setting_row(
                "Search opens with",
                "What Search lists before you type a title.",
                presets.into_any_element(),
            ))
            .child(setting_row(
                "Spoiler tags",
                "Show tags AniList marks as spoilers on show pages. Otherwise each page can reveal them.",
                switch("settings-spoilers", s.show_spoiler_tags)
                    .on_click(self.on(|s| s.show_spoiler_tags = !s.show_spoiler_tags, cx))
                    .into_any_element(),
            ))
            .into_any_element()
    }

    fn render_downloads(&self, cx: &mut Context<Self>) -> AnyElement {
        let changed = self.typed_dir(cx).is_some();
        let current = self.settings.download_dir.clone();
        section("Downloads")
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .py(px(18.))
                    .border_b_1()
                    .border_color(theme::line())
                    .child(label_block(
                        "Download folder",
                        "Where downloaded episodes are saved. Type a full path and press Save.",
                    ))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(div().flex_1().min_w_0().child(self.dir_input.clone()))
                            .child(
                                button(
                                    "settings-dir-save",
                                    if self.saving_dir { "Saving…" } else { "Save" },
                                )
                                .kind(ButtonKind::Secondary)
                                .disabled(!changed || self.saving_dir)
                                .on_click(cx.listener(|page, _, _, cx| page.save_dir(cx))),
                            )
                            .child(
                                button("settings-dir-open", "Open")
                                    .kind(ButtonKind::Ghost)
                                    .on_click(move |_, _, cx| open_folder(current.clone(), cx)),
                            ),
                    )
                    .children(self.dir_error.clone().map(|err| {
                        div()
                            .text_size(px(theme::SMALL))
                            .text_color(theme::danger())
                            .child(err)
                    })),
            )
            .into_any_element()
    }

    fn render_updates(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(updates) = updater::model(cx) else {
            return div().into_any_element();
        };
        let toggle_auto = self.on(|s| s.auto_update = !s.auto_update, cx);
        let u = updates.read(cx);
        let on = self.settings.auto_update;
        let auto_help = match &u.install_path {
            Ok(_) => "Downloads signed releases from GitHub and installs them. \
                      The new version runs the next time you start ani-gui."
                .to_string(),
            Err(why) => format!(
                "{} ani-gui will tell you when a new version is out.",
                why.explain()
            ),
        };
        let last = match (u.saved.last_check, &u.saved.result) {
            (Some(at), Some(result)) => {
                format!("Last checked {}: {result}", updater::ago(at, unix_now()))
            }
            (Some(at), None) => format!("Last checked {}", updater::ago(at, unix_now())),
            _ => "Not checked yet".to_string(),
        };
        let busy = u.busy();
        let status = updater::update_notice(u).map(render_update_notice);
        section("Updates")
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(32.))
                    .py(px(18.))
                    .border_b_1()
                    .border_color(theme::line())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(label_block("Install updates automatically", auto_help)),
                    )
                    .child(switch("settings-auto-update", on).on_click(toggle_auto)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(32.))
                    .py(px(18.))
                    .border_b_1()
                    .border_color(theme::line())
                    .child(div().flex_1().min_w_0().child(label_block(
                        "Version",
                        format!("ani-gui {}. {last}", updater::CURRENT),
                    )))
                    .child(
                        button(
                            "settings-update-check",
                            if matches!(u.status, Status::Checking) {
                                "Checking…"
                            } else {
                                "Check now"
                            },
                        )
                        .kind(ButtonKind::Secondary)
                        .disabled(busy)
                        .on_click(|_, _, cx| updater::check_now(cx)),
                    ),
            )
            .children(status)
            .into_any_element()
    }

    fn render_about(&self) -> AnyElement {
        let logo_w = 150.;
        section("About")
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(28.))
                    .py(px(22.))
                    .border_b_1()
                    .border_color(theme::line())
                    .child(
                        img(theme::stacked_logo())
                            .flex_none()
                            .w(px(logo_w))
                            .h(px(logo_w * theme::LOGO_STACKED_ASPECT))
                            .object_fit(ObjectFit::Contain),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(10.))
                            .child(label_block(
                                "ani-gui",
                                format!(
                                    "Version {}. A desktop app for watching anime with ani-cli. \
                                     Show data comes from AniList, opening times from AniSkip.",
                                    updater::CURRENT
                                ),
                            ))
                            .child(
                                div()
                                    .flex()
                                    .gap(px(8.))
                                    .child(
                                        button("settings-about-site", "Project page")
                                            .kind(ButtonKind::Secondary)
                                            .compact()
                                            .on_click(|_, _, cx| cx.open_url(updater::REPO_URL)),
                                    )
                                    .child(
                                        button("settings-about-anilist", "AniList")
                                            .kind(ButtonKind::Ghost)
                                            .compact()
                                            .on_click(|_, _, cx| cx.open_url("https://anilist.co")),
                                    ),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// MyAnimeList and AniList (`ui::accounts`).
    fn render_accounts(&self) -> AnyElement {
        section("Accounts")
            .child(self.accounts.clone())
            .into_any_element()
    }

    fn render_diagnostics(&self, cx: &mut Context<Self>) -> AnyElement {
        let heading = div()
            .flex()
            .items_center()
            .justify_between()
            .child(section_title("Diagnostics"))
            .child(
                button(
                    "settings-recheck",
                    if self.check_task.is_some() {
                        "Checking…"
                    } else {
                        "Check again"
                    },
                )
                .kind(ButtonKind::Ghost)
                .compact()
                .disabled(self.check_task.is_some())
                .on_click(cx.listener(|page, _, _, cx| page.check(cx))),
            );
        let body = div().flex().flex_col();
        let Some(report) = &self.report else {
            return div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(heading)
                .child(
                    body.py(px(18.))
                        .text_color(theme::text_muted())
                        .child("Checking what's installed…"),
                )
                .into_any_element();
        };
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(heading)
            .child(body.children(checks(report).into_iter().map(check_row)))
            .into_any_element()
    }
}

impl Render for SettingsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("settings")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(40.))
                    .max_w(px(CONTENT_W) + theme::gutter() * 2.)
                    .px(theme::gutter())
                    .pt(px(48.))
                    .pb(px(64.))
                    .child(
                        div()
                            .font_family(theme::DISPLAY_FONT)
                            .font_weight(FontWeight::BOLD)
                            .text_size(px(theme::PAGE_TITLE))
                            .line_height(px(theme::PAGE_TITLE))
                            .child("Settings"),
                    )
                    .child(self.render_appearance(cx))
                    .child(self.render_playback(cx))
                    .child(self.render_browsing(cx))
                    .child(self.render_downloads(cx))
                    .child(self.render_updates(cx))
                    .child(self.render_accounts())
                    .child(self.render_diagnostics(cx))
                    .child(self.render_about()),
            )
    }
}

/// One line of the diagnostics list.
struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
}

fn checks(report: &Report) -> Vec<Check> {
    let tool = |name: &'static str, found: bool, needed_for: &str| Check {
        name,
        ok: found,
        detail: if found {
            "Found on your PATH".into()
        } else {
            format!(
                "Not found on your PATH. ani-cli needs it {needed_for}.{}",
                diagnostics::install_tip(name)
            )
        },
    };
    vec![
        Check {
            name: "ani-cli",
            ok: report.ani_cli.is_some(),
            detail: match &report.ani_cli {
                Some((path, Some(version))) => {
                    format!("Version {version} at {}", display_path(path))
                }
                Some((path, None)) => format!(
                    "Found at {}, but it didn't report a version",
                    display_path(path)
                ),
                None => format!(
                    "Not found. Install ani-cli to stream and download episodes.{}",
                    diagnostics::install_tip("ani-cli")
                ),
            },
        },
        Check {
            name: "libmpv",
            ok: report.libmpv.is_ok(),
            detail: match &report.libmpv {
                Ok(()) => "Loaded, so videos play in this window".into(),
                Err(err) => format!(
                    "Couldn't load it ({err}). {} to play videos.",
                    diagnostics::LIBMPV_INSTALL_HINT
                ),
            },
        },
        tool("yt-dlp", report.ytdlp, "to download some episodes"),
        tool("ffmpeg", report.ffmpeg, "to download some episodes"),
        tool("curl", report.curl, "to find episodes"),
    ]
}

fn check_row(check: Check) -> impl IntoElement {
    div()
        .flex()
        .items_start()
        .gap(px(16.))
        .py(px(14.))
        .border_b_1()
        .border_color(theme::line())
        .child(
            div()
                .flex_none()
                .w(px(96.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(check.name),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(theme::SMALL))
                .line_height(relative(1.5))
                .text_color(if check.ok {
                    theme::text_dim()
                } else {
                    theme::danger()
                })
                .child(check.detail),
        )
        .child(
            div()
                .flex_none()
                .text_size(px(theme::SMALL))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(if check.ok {
                    theme::text_dim()
                } else {
                    theme::danger()
                })
                .child(if check.ok { "Ready" } else { "Missing" }),
        )
}

/// An on/off switch, styled like the autoplay one.
fn switch(id: &'static str, on: bool) -> gpui::Stateful<gpui::Div> {
    let light = theme::is_light();
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .w(px(48.))
        .h(px(28.))
        .p(if light && !on { px(2.) } else { px(3.) })
        .rounded_full()
        .cursor_pointer()
        .bg(if on {
            theme::accent()
        } else if light {
            theme::track()
        } else {
            theme::raised()
        })
        .when(light && !on, |d| {
            d.border_1().border_color(theme::control_border())
        })
        .when(on, |d| d.justify_end())
        .child(
            div()
                .size(px(22.))
                .rounded_full()
                .bg(if light {
                    theme::control()
                } else if on {
                    theme::on_accent()
                } else {
                    theme::text_dim()
                })
                .when(light, |d| d.shadow(theme::shadow_sm())),
        )
}

/// The line under the Updates rows: what was found or went wrong, a link
/// to the release page, and the one action that fits.
fn render_update_notice(notice: UpdateNotice) -> AnyElement {
    let leading: AnyElement = if notice.busy {
        icon("loader-circle")
            .size(px(18.))
            .text_color(theme::accent())
            .with_animation(
                "settings-update-spinner",
                Animation::new(Duration::from_millis(900)).repeat(),
                |svg, delta| svg.with_transformation(Transformation::rotate(percentage(delta))),
            )
            .into_any_element()
    } else {
        let icon_name = if notice.error {
            "circle-alert"
        } else {
            match notice.action.as_ref().map(|(_, a)| a) {
                Some(UpdateAction::Restart) => "check",
                Some(UpdateAction::OpenRelease { .. }) => "external-link",
                _ => "download",
            }
        };
        icon(icon_name)
            .size(px(18.))
            .text_color(if notice.error {
                theme::danger()
            } else {
                theme::accent()
            })
            .into_any_element()
    };
    div()
        .flex()
        .items_center()
        .gap(px(12.))
        .py(px(14.))
        .border_b_1()
        .border_color(theme::line())
        .child(leading)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(
                    div()
                        .text_size(px(theme::SMALL))
                        .line_height(relative(1.5))
                        .text_color(if notice.error {
                            theme::danger()
                        } else {
                            theme::text()
                        })
                        .child(notice.message),
                )
                .children(notice.progress.map(|frac| {
                    div()
                        .h(px(4.))
                        .max_w(px(260.))
                        .w_full()
                        .rounded_full()
                        .bg(theme::track())
                        .overflow_hidden()
                        .child(
                            div()
                                .h_full()
                                .w(relative(frac.clamp(0.0, 1.0)))
                                .bg(theme::accent()),
                        )
                })),
        )
        .child(
            div()
                .flex()
                .flex_none()
                .gap(px(8.))
                .children(notice.release_url.map(|url| {
                    button("settings-update-page", "Release notes")
                        .kind(ButtonKind::Ghost)
                        .compact()
                        .on_click(move |_, _, cx| cx.open_url(&url))
                }))
                .children(notice.action.map(|(label, action)| {
                    let kind = if matches!(action, UpdateAction::Restart) {
                        ButtonKind::Primary
                    } else {
                        ButtonKind::Secondary
                    };
                    button("settings-update-action", label)
                        .kind(kind)
                        .compact()
                        .on_click(move |_, _, cx| updater::perform(action.clone(), cx))
                })),
        )
        .into_any_element()
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn section_title(title: &'static str) -> impl IntoElement {
    div()
        .font_family(theme::DISPLAY_FONT)
        .font_weight(FontWeight::SEMIBOLD)
        .text_size(px(theme::SECTION))
        .child(title)
}

fn section(title: &'static str) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(section_title(title))
}

fn label_block(label: &'static str, help: impl Into<SharedString>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .min_w_0()
        .child(div().font_weight(FontWeight::SEMIBOLD).child(label))
        .child(
            div()
                .text_size(px(theme::SMALL))
                .line_height(relative(1.45))
                .text_color(theme::text_muted())
                .child(help.into()),
        )
}

fn setting_row(
    label: &'static str,
    help: impl Into<SharedString>,
    control: AnyElement,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(32.))
        .py(px(18.))
        .border_b_1()
        .border_color(theme::line())
        .child(div().flex_1().min_w_0().child(label_block(label, help)))
        .child(control)
}

/// The download folder typed by the user: absolute, with "~" for `home`.
fn parse_dir(typed: &str, home: Option<&Path>) -> Result<PathBuf, String> {
    let typed = typed.trim();
    let dir = match (typed.strip_prefix('~'), home) {
        (Some(""), Some(home)) => home.to_path_buf(),
        (Some(rest), Some(home)) if rest.starts_with('/') => {
            home.join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(typed),
    };
    if typed.is_empty() {
        return Err("Type the folder to save downloads in.".into());
    }
    if !dir.is_absolute() {
        return Err("Use a full path, starting with / or ~/.".into());
    }
    Ok(dir)
}

/// Creates `dir` if needed and checks a file can be written in it.
fn prepare_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let probe = dir.join(format!(".ani-gui-write-test-{}", std::process::id()));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)?;
    std::fs::remove_file(&probe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_dir_needs_a_writable_folder() {
        let dir = std::env::temp_dir().join(format!("ani-gui-prep-{}", std::process::id()));
        let nested = dir.join("a/b");
        prepare_dir(&nested).unwrap();
        assert!(nested.is_dir());
        // The probe file is gone afterwards.
        assert_eq!(std::fs::read_dir(&nested).unwrap().count(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
        // System folders aren't writable by a normal user.
        // SAFETY: geteuid has no preconditions.
        if unsafe { nix::libc::geteuid() } != 0 {
            assert!(prepare_dir(Path::new("/")).is_err());
            assert!(prepare_dir(Path::new("/usr")).is_err());
        }
    }

    #[test]
    fn parse_dir_expands_home_and_needs_full_paths() {
        let home = Path::new("/home/k");
        assert_eq!(
            parse_dir("~/Videos/anime ", Some(home)),
            Ok(PathBuf::from("/home/k/Videos/anime"))
        );
        assert_eq!(parse_dir("~", Some(home)), Ok(PathBuf::from("/home/k")));
        assert_eq!(
            parse_dir("/mnt/media", Some(home)),
            Ok(PathBuf::from("/mnt/media"))
        );
        assert!(parse_dir("Videos", Some(home)).is_err());
        assert!(parse_dir("  ", Some(home)).is_err());
        assert!(parse_dir("~other/x", Some(home)).is_err());
    }
}
