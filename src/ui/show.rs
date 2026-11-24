use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, anyhow};
use gpui::{
    AnyElement, ClickEvent, Context, Entity, FontWeight, IntoElement, ScrollHandle, SharedString,
    Subscription, Task, WeakEntity, Window, div, linear_color_stop, linear_gradient, prelude::*,
    px, relative,
};

use crate::anicli::{ResolveOutcome, ResolveScope};
use crate::app::{AppState, PlayRequest, Root, ShowRef};
use crate::db::{self, Db, lock};
use crate::downloads::DownloadManager;
use crate::model::{AniListMedia, DlStatus, Mode, Progress, ShowRecord, ShowSummary};
use crate::ui::components::button::{ButtonKind, button, segmented};
use crate::ui::components::image::{remote_image, rounded_image, shimmer};
use crate::ui::components::pick_dialog::pick_dialog;
use crate::ui::components::poster::progress_bar;
use crate::ui::components::text_input::TextInput;
use crate::ui::components::toast;
use crate::ui::theme;
use crate::ui::view_models::{self, EpisodeRow, ShowPageModel, media_title, mode_label};
use crate::{allanime, catalog, playback};

pub const QUALITIES: [(&str, &str); 4] = [
    ("best", "Best"),
    ("1080", "1080p"),
    ("720", "720p"),
    ("480", "480p"),
];
/// Long shows list their episodes a hundred at a time.
const EPISODE_PAGE: usize = 100;
const BANNER_H: f32 = 340.;
const COVER_W: f32 = 200.;
const COVER_H: f32 = 300.;
/// About four lines of the 720 px synopsis.
const SYNOPSIS_CHARS: usize = 360;

struct Loaded {
    show: ShowSummary,
    media: Option<AniListMedia>,
    episodes: Vec<String>,
    progress: Vec<Progress>,
    in_list: bool,
    /// Why the episode list could not be fetched.
    episodes_error: Option<String>,
    /// The ani-cli title this show was matched to, if one is remembered.
    provider_title: Option<String>,
}

/// Where an episode to play was found.
enum Found {
    File(PathBuf),
    Resolved(ResolveOutcome),
}

/// A download's state for one episode of this show.
struct EpisodeDownload {
    id: String,
    status: DlStatus,
    percent: f32,
    error: Option<String>,
}

/// ani-cli's candidate titles, awaiting the user's choice for one play.
struct Pick {
    candidates: Vec<String>,
    ep: String,
    start: f64,
}

pub struct ShowPage {
    root: WeakEntity<Root>,
    target: ShowRef,
    /// The mode everything shown is in.
    mode: Mode,
    /// A mode being switched to; `mode` changes once it has loaded.
    loading_mode: Option<Mode>,
    quality: String,
    loaded: Option<Loaded>,
    load_error: Option<String>,
    model: ShowPageModel,
    page: usize,
    downloads: HashMap<String, EpisodeDownload>,
    /// Episodes whose downloads are being started, by mode.
    queuing: HashSet<(Mode, String)>,
    /// Episode being resolved for playback.
    resolving: Option<String>,
    /// Kills this page's ani-cli runs when it goes away.
    resolves: ResolveScope,
    play_error: Option<String>,
    pick: Option<Pick>,
    load_task: Option<Task<()>>,
    sync_task: Option<Task<()>>,
    range_from: Entity<TextInput>,
    range_to: Entity<TextInput>,
    range_error: Option<String>,
    scroll: ScrollHandle,
    _downloads_changed: Option<Subscription>,
}

impl ShowPage {
    pub fn new(target: ShowRef, root: WeakEntity<Root>, cx: &mut Context<Self>) -> Self {
        let (mode, quality, watch) = match cx.try_global::<AppState>() {
            Some(s) => (
                s.settings.mode,
                s.settings.quality.clone(),
                Some(s.downloads_changed.clone()),
            ),
            None => (Mode::default(), "best".to_string(), None),
        };
        let subscription = watch.map(|w| cx.observe(&w, |page, _, cx| page.sync_downloads(cx)));
        let mut page = ShowPage {
            root,
            target,
            mode,
            loading_mode: None,
            quality,
            loaded: None,
            load_error: None,
            model: ShowPageModel::default(),
            page: 0,
            downloads: HashMap::new(),
            queuing: HashSet::new(),
            resolving: None,
            resolves: ResolveScope::default(),
            play_error: None,
            pick: None,
            load_task: None,
            sync_task: None,
            range_from: cx.new(|cx| TextInput::new("From", cx).compact()),
            range_to: cx.new(|cx| TextInput::new("To", cx).compact()),
            range_error: None,
            scroll: ScrollHandle::new(),
            _downloads_changed: subscription,
        };
        page.reload(cx);
        page
    }

    fn state<'a>(&self, cx: &'a Context<Self>) -> Option<&'a AppState> {
        cx.try_global::<AppState>()
    }

    /// (Re)loads the show, its episodes in the current mode and progress.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.load(self.mode, cx);
    }

    /// Loads everything in `mode` in the background; the page switches to
    /// `mode` only once it has all of it.
    fn load(&mut self, mode: Mode, cx: &mut Context<Self>) {
        let Some(state) = self.state(cx) else {
            return;
        };
        let (db, manager) = (state.db.clone(), state.downloads.clone());
        let mut target = self.target.clone();
        if let Some(loaded) = &self.loaded {
            target.allanime = Some(loaded.show.clone());
        }
        self.load_error = None;
        self.loading_mode = (mode != self.mode).then_some(mode);
        let task = cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    let loaded = load(&db, &target, mode)?;
                    let downloads = episode_downloads(&manager, &loaded.show.id, mode);
                    anyhow::Ok((loaded, downloads))
                })
                .await;
            this.update(cx, |page, cx| {
                page.load_task = None;
                page.loading_mode = None;
                match loaded {
                    Ok((loaded, downloads)) => {
                        if page.mode != mode {
                            page.play_error = None;
                        }
                        page.mode = mode;
                        page.loaded = Some(loaded);
                        page.downloads = downloads;
                        page.rebuild(cx);
                    }
                    Err(err) if page.loaded.is_some() => {
                        toast::error(cx, format!("Couldn't load {mode} episodes: {err:#}"));
                    }
                    Err(err) => page.load_error = Some(format!("{err:#}")),
                }
                cx.notify();
            })
            .ok();
        });
        self.load_task = Some(task);
        cx.notify();
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let Some(l) = &self.loaded else {
            return;
        };
        let episodes = l.episodes_error.is_none().then_some(l.episodes.as_slice());
        self.model =
            ShowPageModel::build(&l.show, l.media.as_ref(), episodes, &l.progress, self.mode);
        let (from, to) =
            view_models::default_download_range(&self.model.episodes).unwrap_or_default();
        self.range_from
            .update(cx, |input, cx| input.set_text(from, cx));
        self.range_to.update(cx, |input, cx| input.set_text(to, cx));
        self.range_error = None;
        let target_index = self
            .model
            .play_target
            .as_ref()
            .and_then(|(ep, _)| l.episodes.iter().position(|e| e == ep))
            .unwrap_or(0);
        self.page = target_index / EPISODE_PAGE;
    }

    fn sync_downloads(&mut self, cx: &mut Context<Self>) {
        let (Some(state), Some(loaded)) = (self.state(cx), &self.loaded) else {
            return;
        };
        let manager = state.downloads.clone();
        let (show_id, mode) = (loaded.show.id.clone(), self.mode);
        self.sync_task = Some(cx.spawn(async move |this, cx| {
            let id = show_id.clone();
            let downloads = cx
                .background_spawn(async move { episode_downloads(&manager, &id, mode) })
                .await;
            this.update(cx, |page, cx| {
                if page.mode == mode && page.loaded.as_ref().is_some_and(|l| l.show.id == show_id) {
                    page.downloads = downloads;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if self.loading_mode.unwrap_or(self.mode) != mode {
            self.load(mode, cx);
        }
    }

    fn toggle_list(&mut self, cx: &mut Context<Self>) {
        let Some(db) = self.state(cx).map(|s| s.db.clone()) else {
            return;
        };
        let Some(loaded) = self.loaded.as_mut() else {
            return;
        };
        let add = !loaded.in_list;
        loaded.in_list = add;
        let id = loaded.show.id.clone();
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
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
                    if let Some(loaded) = page.loaded.as_mut() {
                        loaded.in_list = !add;
                    }
                    toast::error(cx, format!("Couldn't update My List: {err:#}"));
                    cx.notify();
                })
                .ok();
            }
        });
        task.detach();
    }

    /// Opens the player on `ep`: its downloaded file when there is one,
    /// else the stream ani-cli resolves. With `chosen`, that title (picked
    /// from ani-cli's results) becomes the show's provider title first, so
    /// this and later runs search for it.
    fn play(&mut self, ep: String, start: f64, chosen: Option<String>, cx: &mut Context<Self>) {
        if self.resolving.is_some() {
            return;
        }
        let Some(state) = self.state(cx) else {
            return;
        };
        let Some(loaded) = &self.loaded else {
            return;
        };
        let shims = state.shims.clone();
        let (db, manager) = (state.db.clone(), state.downloads.clone());
        let show = loaded.show.clone();
        let episodes = loaded.episodes.clone();
        let (mode, quality) = (self.mode, self.quality.clone());
        let owner = self.resolves.handle();
        self.resolving = Some(ep.clone());
        self.play_error = None;
        self.pick = None;
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let (show_bg, ep_bg, quality_bg) = (show.clone(), ep.clone(), quality.clone());
            let found = cx
                .background_spawn(async move {
                    if let Some(file) = downloaded_file(&manager, &show_bg.id, mode, &ep_bg) {
                        return Ok(Found::File(file));
                    }
                    let shims = shims.map_err(|err| anyhow!(err))?;
                    playback::resolve(
                        &db,
                        &shims,
                        &show_bg,
                        &ep_bg,
                        mode,
                        &quality_bg,
                        chosen.as_deref(),
                        &owner,
                    )
                    .map(Found::Resolved)
                })
                .await;
            this.update(cx, |page, cx| {
                page.resolving = None;
                let from = cx.entity_id();
                let play = |local_file, stream| PlayRequest {
                    show,
                    mode,
                    ep: ep.clone(),
                    start,
                    episodes,
                    quality,
                    local_file,
                    stream,
                };
                match found {
                    Ok(Found::File(file)) => {
                        let req = play(Some(file), None);
                        page.root
                            .update(cx, |root, cx| root.play_from(from, req, cx))
                            .ok();
                    }
                    Ok(Found::Resolved(ResolveOutcome::Stream(stream))) => {
                        let req = play(None, Some(stream));
                        page.root
                            .update(cx, |root, cx| root.play_from(from, req, cx))
                            .ok();
                    }
                    Ok(Found::Resolved(ResolveOutcome::NeedsPick(candidates))) => {
                        page.pick = Some(Pick {
                            candidates,
                            ep,
                            start,
                        });
                    }
                    Err(err) => {
                        page.play_error = Some(format!("Couldn't start episode {ep}: {err:#}"));
                    }
                }
                cx.notify();
            })
            .ok();
        });
        task.detach();
    }

    fn choose(&mut self, title: String, cx: &mut Context<Self>) {
        if let Some(pick) = self.pick.take() {
            if let Some(loaded) = self.loaded.as_mut() {
                loaded.provider_title = Some(title.clone());
            }
            self.play(pick.ep, pick.start, Some(title), cx);
        }
    }

    /// "Wrong show?": forgets the remembered ani-cli title, so the next
    /// play asks which of ani-cli's results this show is.
    fn choose_again(&mut self, cx: &mut Context<Self>) {
        let Some(db) = self.state(cx).map(|s| s.db.clone()) else {
            return;
        };
        let Some(loaded) = self.loaded.as_mut() else {
            return;
        };
        let Some(previous) = loaded.provider_title.take() else {
            return;
        };
        let id = loaded.show.id.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let cleared = cx
                .background_spawn(async move { playback::forget_provider_title(&db, &id) })
                .await;
            this.update(cx, |page, cx| {
                match cleared {
                    Ok(()) => toast::info(cx, "You'll choose the right show next time you play"),
                    Err(err) => {
                        if let Some(loaded) = page.loaded.as_mut() {
                            loaded.provider_title = Some(previous);
                        }
                        toast::error(cx, format!("Couldn't forget the match: {err:#}"));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn download(&mut self, ep: String, cx: &mut Context<Self>) {
        self.queue(vec![ep], cx);
    }

    fn download_range(&mut self, cx: &mut Context<Self>) {
        let Some(loaded) = &self.loaded else {
            return;
        };
        let from = self.range_from.read(cx).text().to_string();
        let to = self.range_to.read(cx).text().to_string();
        match view_models::download_range(&loaded.episodes, &from, &to) {
            Ok(eps) => {
                self.range_error = None;
                self.queue(eps, cx);
            }
            Err(err) => self.range_error = Some(err),
        }
        cx.notify();
    }

    /// Starts (or retries) downloads of `eps` in the background, skipping
    /// episodes already downloaded or on their way.
    fn queue(&mut self, eps: Vec<String>, cx: &mut Context<Self>) {
        let Some(state) = self.state(cx) else {
            return;
        };
        let Some(loaded) = &self.loaded else {
            return;
        };
        let mode = self.mode;
        let jobs: Vec<(String, Option<String>)> = eps
            .into_iter()
            .filter(|ep| !self.queuing.contains(&(mode, ep.clone())))
            .filter_map(|ep| match self.downloads.get(&ep) {
                None => Some((ep, None)),
                Some(d) if matches!(d.status, DlStatus::Failed | DlStatus::Cancelled) => {
                    let id = d.id.clone();
                    Some((ep, Some(id)))
                }
                Some(_) => None,
            })
            .collect();
        let title = loaded.show.display_title().to_string();
        let label = match jobs.as_slice() {
            [] => {
                toast::info(cx, "Those episodes are already downloaded or queued");
                return;
            }
            [(ep, _)] => format!("Downloading {title} episode {ep}"),
            many => format!("Downloading {} episodes of {title}", many.len()),
        };
        let db = state.db.clone();
        let manager = state.downloads.clone();
        let dir = state.settings.download_dir.clone();
        let show = loaded.show.clone();
        let quality = self.quality.clone();
        let started: Vec<(Mode, String)> = jobs.iter().map(|(ep, _)| (mode, ep.clone())).collect();
        self.queuing.extend(started.iter().cloned());
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let show_id = show.id.clone();
            let (downloads, failed) = cx
                .background_spawn(async move {
                    let mut failed = None;
                    for (ep, retry_id) in jobs {
                        let started = start_download(
                            &db, &manager, &show, &ep, mode, &quality, &dir, retry_id,
                        );
                        if let Err(err) = started {
                            failed
                                .get_or_insert(format!("Couldn't download episode {ep}: {err:#}"));
                        }
                    }
                    (episode_downloads(&manager, &show.id, mode), failed)
                })
                .await;
            this.update(cx, |page, cx| {
                for key in &started {
                    page.queuing.remove(key);
                }
                if page.mode == mode && page.loaded.as_ref().is_some_and(|l| l.show.id == show_id) {
                    page.downloads = downloads;
                }
                match failed {
                    Some(err) => toast::error(cx, err),
                    None => toast::info(cx, label),
                }
                cx.notify();
            })
            .ok();
        });
        task.detach();
    }

    fn back(&self, cx: &mut Context<Self>) {
        self.root.update(cx, |root, cx| root.back(cx)).ok();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let loaded = self.loaded.as_ref();
        let media = loaded.and_then(|l| l.media.as_ref());
        let title = match (media, loaded) {
            (Some(m), _) => media_title(m).to_string(),
            (None, Some(l)) => l.show.display_title().to_string(),
            (None, None) => self.target.title.clone(),
        };
        let banner = media
            .and_then(|m| m.banner_url.clone())
            .or_else(|| loaded.map(|l| l.show.cover_url.clone()));
        let cover = media
            .map(|m| m.cover_url.clone())
            .filter(|c| !c.is_empty())
            .or_else(|| loaded.map(|l| l.show.cover_url.clone()));
        let mut meta: Vec<String> = Vec::new();
        if let Some(m) = media {
            meta.extend(m.year.map(|y| y.to_string()));
            meta.extend(m.format.clone());
            meta.extend(m.episodes.map(|n| format!("{n} episodes")));
            meta.extend(m.score.map(|s| format!("Score {s}")));
            meta.extend(m.status.as_deref().map(status_label));
        }
        let synopsis = media.and_then(|m| m.synopsis.clone());
        let genres = media.map(|m| m.genres.clone()).unwrap_or_default();

        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(px(BANNER_H))
                    .overflow_hidden()
                    .child(div().absolute().inset_0().child(match &banner {
                        Some(url) => remote_image(url).into_any_element(),
                        None => shimmer().into_any_element(),
                    }))
                    .child(div().absolute().inset_0().bg(linear_gradient(
                        0.,
                        linear_color_stop(theme::night_alpha(1.), 0.),
                        linear_color_stop(theme::night_alpha(0.35), 0.75),
                    )))
                    .child(
                        div().absolute().top(px(20.)).left(px(24.)).child(
                            button("show-back", "‹ Back")
                                .kind(ButtonKind::Secondary)
                                .compact()
                                .on_click(cx.listener(|page, _, _, cx| page.back(cx))),
                        ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap(px(36.))
                    .px(theme::gutter())
                    .mt(px(-130.))
                    .child(
                        div()
                            .flex_none()
                            .w(px(COVER_W))
                            .h(px(COVER_H))
                            .rounded(px(12.))
                            .shadow_lg()
                            .child(match &cover {
                                Some(url) => rounded_image(url, px(12.), theme::night(), None)
                                    .into_any_element(),
                                None => div()
                                    .size_full()
                                    .rounded(px(12.))
                                    .overflow_hidden()
                                    .child(shimmer())
                                    .into_any_element(),
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(14.))
                            .child(
                                div()
                                    .font_family(theme::DISPLAY_FONT)
                                    .font_weight(FontWeight::BOLD)
                                    .text_size(px(theme::PAGE_TITLE))
                                    .line_height(relative(1.))
                                    .line_clamp(2)
                                    .child(title),
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
                                    .children(genres.into_iter().take(5).map(|g| {
                                        div()
                                            .px(px(10.))
                                            .py(px(3.))
                                            .rounded(px(6.))
                                            .border_1()
                                            .border_color(theme::line())
                                            .child(g)
                                    })),
                            )
                            .children(synopsis.map(|s| {
                                div()
                                    .max_w(px(720.))
                                    .text_size(px(theme::BODY))
                                    .line_height(relative(1.55))
                                    .text_color(theme::text_dim())
                                    .line_clamp(4)
                                    .child(view_models::clamp_words(
                                        &view_models::blurb(&s),
                                        SYNOPSIS_CHARS,
                                    ))
                            }))
                            .child(self.render_controls(cx)),
                    ),
            )
            .into_any_element()
    }

    fn render_controls(&self, cx: &mut Context<Self>) -> AnyElement {
        let ready = self.loaded.is_some();
        let resolving = self.resolving.is_some();
        let play_label: SharedString = if resolving {
            "Finding stream…".into()
        } else if ready {
            self.model.play_label.clone().into()
        } else {
            "Loading…".into()
        };
        let target = self.model.play_target.clone();
        let in_list = self.loaded.as_ref().is_some_and(|l| l.in_list);
        let mode = self.loading_mode.unwrap_or(self.mode);
        let mut quality = segmented("show-quality");
        for (value, label) in QUALITIES {
            quality = quality.option(
                label,
                self.quality == value,
                cx.listener(move |page, _, _, cx| {
                    page.quality = value.to_string();
                    cx.notify();
                }),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .pt(px(6.))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        button("show-play", play_label)
                            .disabled(target.is_none() || resolving)
                            .on_click(cx.listener(move |page, _, _, cx| {
                                if let Some((ep, start)) = target.clone() {
                                    page.play(ep, start, None, cx);
                                }
                            })),
                    )
                    .children(self.model.other_mode.filter(|_| ready).map(|other| {
                        button(
                            "show-switch-mode",
                            format!("Watch in {}", mode_label(other)),
                        )
                        .kind(ButtonKind::Secondary)
                        .on_click(cx.listener(move |page, _, _, cx| page.set_mode(other, cx)))
                    }))
                    .child(
                        button(
                            "show-list",
                            if in_list { "In My List" } else { "+ My List" },
                        )
                        .kind(ButtonKind::Secondary)
                        .disabled(!ready)
                        .on_click(cx.listener(|page, _, _, cx| page.toggle_list(cx))),
                    )
                    .child(
                        segmented("show-mode")
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
                    )
                    .child(quality),
            )
            .children(self.play_error.clone().map(|err| {
                div()
                    .max_w(px(720.))
                    .text_size(px(theme::SMALL))
                    .line_height(relative(1.45))
                    .text_color(theme::danger())
                    .child(err)
            }))
            .children(
                self.loaded
                    .as_ref()
                    .and_then(|l| l.provider_title.clone())
                    .map(|title| {
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(10.))
                            .text_size(px(theme::SMALL))
                            .text_color(theme::text_muted())
                            .child(format!("Plays as “{title}”"))
                            .child(
                                button("show-choose-again", "Wrong show? Choose again")
                                    .kind(ButtonKind::Ghost)
                                    .compact()
                                    .disabled(resolving)
                                    .on_click(cx.listener(|page, _, _, cx| page.choose_again(cx))),
                            )
                    }),
            )
            .into_any_element()
    }

    fn render_episodes(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(loaded) = &self.loaded else {
            return div().into_any_element();
        };
        let total = self.model.episodes.len();
        let pages = total.div_ceil(EPISODE_PAGE);
        let start = (self.page * EPISODE_PAGE).min(total);
        let end = (start + EPISODE_PAGE).min(total);
        let heading = div()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap(px(12.))
                    .child(
                        div()
                            .font_family(theme::DISPLAY_FONT)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(px(theme::SECTION))
                            .child("Episodes"),
                    )
                    .child(
                        div()
                            .text_size(px(theme::SMALL))
                            .text_color(theme::text_muted())
                            .child(format!("{total} in {}", mode_label(self.mode))),
                    ),
            )
            .when(pages > 1, |h| {
                h.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(6.))
                        .children((0..pages).map(|p| {
                            let first = p * EPISODE_PAGE;
                            let last = ((p + 1) * EPISODE_PAGE).min(total) - 1;
                            let label = format!(
                                "{}–{}",
                                self.model.episodes[first].ep, self.model.episodes[last].ep
                            );
                            button(("ep-page", p), label)
                                .kind(if p == self.page {
                                    ButtonKind::Secondary
                                } else {
                                    ButtonKind::Ghost
                                })
                                .compact()
                                .on_click(cx.listener(move |page, _, _, cx| {
                                    page.page = p;
                                    cx.notify();
                                }))
                        })),
                )
            });
        let body: AnyElement = if let Some(err) = &loaded.episodes_error {
            div()
                .flex()
                .flex_col()
                .items_start()
                .gap(px(12.))
                .child(
                    div()
                        .max_w(px(720.))
                        .text_color(theme::danger())
                        .child(format!("Couldn't load the episode list: {err}")),
                )
                .child(
                    button("episodes-retry", "Try again")
                        .kind(ButtonKind::Secondary)
                        .compact()
                        .disabled(self.load_task.is_some())
                        .on_click(cx.listener(|page, _, _, cx| page.reload(cx))),
                )
                .into_any_element()
        } else if total == 0 {
            div()
                .text_color(theme::text_dim())
                .child(match self.model.other_mode {
                    Some(other) => format!(
                        "No {} episodes yet. It's available in {}.",
                        mode_label(self.mode),
                        mode_label(other)
                    ),
                    None => format!("No {} episodes yet.", mode_label(self.mode)),
                })
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .children(
                    self.model.episodes[start..end]
                        .iter()
                        .enumerate()
                        .map(|(i, row)| self.render_episode(start + i, row, cx)),
                )
                .into_any_element()
        };
        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .px(theme::gutter())
            .pt(px(48.))
            .pb(px(72.))
            .child(heading)
            .when(total > 0, |d| d.child(self.render_range(cx)))
            .child(body)
            .into_any_element()
    }

    fn render_range(&self, cx: &mut Context<Self>) -> AnyElement {
        let field = |input: &Entity<TextInput>| div().w(px(76.)).child(input.clone());
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(10.))
            .text_size(px(theme::SMALL))
            .text_color(theme::text_dim())
            .child("Download episodes")
            .child(field(&self.range_from))
            .child("to")
            .child(field(&self.range_to))
            .child(
                button("download-range", "Download")
                    .kind(ButtonKind::Secondary)
                    .compact()
                    .on_click(cx.listener(|page, _, _, cx| page.download_range(cx))),
            )
            .children(
                self.range_error
                    .clone()
                    .map(|err| div().text_color(theme::danger()).child(err)),
            )
            .into_any_element()
    }

    fn render_episode(&self, index: usize, row: &EpisodeRow, cx: &mut Context<Self>) -> AnyElement {
        let ep = row.ep.clone();
        let resume = row.start;
        let resolving_this = self.resolving.as_deref() == Some(row.ep.as_str());
        let download = self.downloads.get(&row.ep);
        let queuing = self.queuing.contains(&(self.mode, row.ep.clone()));
        let (dl_label, dl_enabled): (SharedString, bool) = match download.map(|d| d.status) {
            _ if queuing => ("Queued".into(), false),
            None => ("Download".into(), true),
            Some(DlStatus::Queued) => ("Queued".into(), false),
            Some(DlStatus::Running) => (
                format!("{:.0}%", download.map_or(0.0, |d| d.percent)).into(),
                false,
            ),
            Some(DlStatus::Done) => ("Downloaded".into(), false),
            Some(DlStatus::Failed | DlStatus::Cancelled) => ("Retry download".into(), true),
        };
        let dl_error = download
            .filter(|d| d.status == DlStatus::Failed)
            .and_then(|d| d.error.clone());
        let thumb = div()
            .relative()
            .flex_none()
            .w(px(176.))
            .h(px(99.))
            .rounded(px(8.))
            .bg(theme::raised())
            .child(match &row.thumb {
                Some(url) => rounded_image(
                    url,
                    px(8.),
                    theme::night(),
                    Some(("episode", theme::raised().into())),
                )
                .into_any_element(),
                None => div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .font_family(theme::DISPLAY_FONT)
                    .font_weight(FontWeight::BOLD)
                    .text_size(px(40.))
                    .text_color(theme::text_muted())
                    .child(row.ep.clone())
                    .into_any_element(),
            })
            .when(row.fraction > 0.0, |t| {
                t.child(
                    progress_bar(row.fraction)
                        .absolute()
                        .bottom(px(8.))
                        .left(px(8.))
                        .right(px(8.)),
                )
            });
        let play_ep = ep.clone();
        div()
            .id(("episode", index))
            .group("episode")
            .flex()
            .items_center()
            .gap(px(20.))
            .p(px(10.))
            .rounded(px(12.))
            .cursor_pointer()
            .hover(|s| s.bg(theme::raised()))
            .on_click(
                cx.listener(move |page, _, _, cx| page.play(play_ep.clone(), resume, None, cx)),
            )
            .child(thumb)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(format!("Episode {}", row.ep))
                            .when(row.completed, |d| {
                                d.child(
                                    div()
                                        .text_size(px(12.))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(theme::text_muted())
                                        .child("Watched"),
                                )
                            })
                            .when(resolving_this, |d| {
                                d.child(
                                    div()
                                        .text_size(px(12.))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(theme::accent())
                                        .child("Finding stream…"),
                                )
                            }),
                    )
                    .children(row.title.clone().map(|t| {
                        div()
                            .text_size(px(14.))
                            .text_color(theme::text_dim())
                            .truncate()
                            .child(t)
                    }))
                    .children(dl_error.map(|e| {
                        div()
                            .text_size(px(12.))
                            .text_color(theme::danger())
                            .truncate()
                            .child(format!("Download failed: {e}"))
                    })),
            )
            .child(
                div()
                    .id(("download-slot", index))
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(
                        button(("download", index), dl_label)
                            .kind(ButtonKind::Ghost)
                            .compact()
                            .disabled(!dl_enabled)
                            .on_click(cx.listener(move |page, _, _, cx| {
                                cx.stop_propagation();
                                page.download(ep.clone(), cx)
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_pick(&self, pick: &Pick, cx: &mut Context<Self>) -> AnyElement {
        let title = self
            .loaded
            .as_ref()
            .map(|l| l.show.display_title().to_string())
            .unwrap_or_else(|| self.target.title.clone());
        let dismiss = |page: &mut Self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>| {
            page.pick = None;
            cx.notify();
        };
        pick_dialog(
            &title,
            &pick.candidates,
            cx.listener(|page, title: &str, _, cx| page.choose(title.to_string(), cx)),
            cx.listener(dismiss),
            Some(Box::new(cx.listener(dismiss))),
        )
    }
}

impl Render for ShowPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content: AnyElement = match (&self.loaded, &self.load_error) {
            (None, Some(err)) => div()
                .flex()
                .flex_col()
                .gap_4()
                .px(theme::gutter())
                .pt(px(120.))
                .child(
                    div()
                        .font_family(theme::DISPLAY_FONT)
                        .font_weight(FontWeight::BOLD)
                        .text_size(px(theme::PAGE_TITLE))
                        .child(self.target.title.clone()),
                )
                .child(
                    div()
                        .max_w(px(720.))
                        .text_color(theme::danger())
                        .child(format!("Couldn't open this show: {err}")),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(10.))
                        .child(
                            button("show-retry", "Try again")
                                .on_click(cx.listener(|page, _, _, cx| page.reload(cx))),
                        )
                        .child(
                            button("show-error-back", "Back")
                                .kind(ButtonKind::Secondary)
                                .on_click(cx.listener(|page, _, _, cx| page.back(cx))),
                        ),
                )
                .into_any_element(),
            _ => div()
                .flex()
                .flex_col()
                .child(self.render_header(cx))
                .child(self.render_episodes(cx))
                .into_any_element(),
        };
        div()
            .relative()
            .size_full()
            .child(
                div()
                    .id("show")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(content),
            )
            .children(self.pick.as_ref().map(|pick| self.render_pick(pick, cx)))
    }
}

fn status_label(status: &str) -> String {
    match status {
        "RELEASING" => "Airing".into(),
        "FINISHED" => "Finished".into(),
        "NOT_YET_RELEASED" => "Upcoming".into(),
        "CANCELLED" => "Cancelled".into(),
        "HIATUS" => "On hiatus".into(),
        other => other.to_string(),
    }
}

/// The AllAnime show (resolved from AniList when only that is known), its
/// AniList details, and its episodes and progress in `mode`. Saves the show.
fn load(db: &Mutex<Db>, target: &ShowRef, mode: Mode) -> Result<Loaded> {
    let (show, media) = match &target.allanime {
        Some(show) => {
            let media = show
                .anilist_id
                .or(target.anilist_id)
                .and_then(|id| catalog::media(db, id));
            (show.clone(), media)
        }
        None => {
            let id = target.anilist_id.context("nothing identifies this show")?;
            let media = catalog::media(db, id).context("AniList didn't return this show")?;
            // Sub lists nearly everything, so the match works whatever `mode` is.
            let show = catalog::resolve_allanime(&media, Mode::Sub)?
                .ok_or_else(|| anyhow!("{} isn't on AllAnime", media_title(&media)))?;
            (show, Some(media))
        }
    };
    let stored = lock(db).get_show(&show.id).ok().flatten();
    let media = media.or_else(|| stored.and_then(|s| s.media));
    lock(db).upsert_show(&ShowRecord {
        summary: show.clone(),
        media: media.clone(),
        updated_at: db::now(),
    })?;
    let (episodes, episodes_error) = if show.count(mode) == 0 {
        (Vec::new(), None)
    } else {
        match allanime::episodes(&show.id, mode) {
            Ok(eps) => (eps, None),
            Err(err) => (Vec::new(), Some(format!("{err:#}"))),
        }
    };
    let db = lock(db);
    let progress = db.progress_for_show(&show.id, mode)?;
    let in_list = db.my_list_contains(&show.id)?;
    let provider_title = db.get_provider_title(&show.id)?;
    Ok(Loaded {
        show,
        media,
        episodes,
        progress,
        in_list,
        episodes_error,
        provider_title,
    })
}

#[allow(clippy::too_many_arguments)]
fn start_download(
    db: &Mutex<Db>,
    manager: &Arc<DownloadManager>,
    show: &ShowSummary,
    ep: &str,
    mode: Mode,
    quality: &str,
    dir: &Path,
    retry_id: Option<String>,
) -> Result<()> {
    if let Some(id) = retry_id
        && manager.retry(&id).is_ok()
    {
        return Ok(());
    }
    playback::start_download(db, manager, show, ep, mode, quality, dir).map(|_| ())
}

/// The downloaded file of `ep`, if it finished and is still on disk. Reads
/// the manager's state and the disk, so call it off the UI thread.
fn downloaded_file(
    manager: &DownloadManager,
    show_id: &str,
    mode: Mode,
    ep: &str,
) -> Option<PathBuf> {
    let records: Vec<_> = manager.snapshot().into_iter().map(|(r, _)| r).collect();
    playback::downloaded_file(&records, show_id, mode, ep)
}

/// This show's downloads in `mode`, by episode. Reads the manager's state,
/// so call it off the UI thread.
fn episode_downloads(
    manager: &DownloadManager,
    show_id: &str,
    mode: Mode,
) -> HashMap<String, EpisodeDownload> {
    let mut downloads = HashMap::new();
    // Newest first: the first record per episode wins.
    for (record, live) in manager.snapshot() {
        if record.show_id != show_id || record.mode != mode {
            continue;
        }
        downloads
            .entry(record.ep.clone())
            .or_insert(EpisodeDownload {
                id: record.id,
                status: record.status,
                percent: live.and_then(|p| p.percent).unwrap_or(record.progress),
                error: record.error,
            });
    }
    downloads
}
