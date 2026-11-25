//! The in-window player: finds the episode's stream when the caller hasn't,
//! plays it with libmpv and draws mpv's frames under a controls overlay.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::mpsc::{self, UnboundedSender};
use gpui::{
    Animation, AnimationExt, AnyElement, App, Bounds, Context, CursorStyle, Div, ElementId,
    FocusHandle, FontWeight, Hsla, KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent,
    NavigationDirection, ObjectFit, Pixels, RenderImage, ScrollStrategy, SharedString, Task,
    UniformListScrollHandle, WeakEntity, Window, actions, canvas, deferred, div, ease_in_out, img,
    linear_color_stop, linear_gradient, prelude::*, px, relative, uniform_list,
};
use image::RgbaImage;

use crate::anicli::{ResolveOutcome, ResolveScope};
use crate::app::{AppState, Back, PlayRequest, Root};
use crate::catalog;
use crate::db::{self, Db};
use crate::diagnostics::LIBMPV_INSTALL_HINT;
use crate::downloads::DownloadManager;
use crate::images;
use crate::keep_awake::KeepAwake;
use crate::model::{AniListMedia, DownloadRecord, Progress, ShowSummary, Stream};
use crate::paths;
use crate::playback;
use crate::player::{MpvPlayer, PlaybackState, PlayerError};
use crate::skip_times::{self, ActivePrompt, AniSkipEntry, Chapter, PromptTracker, SkipTimes};
use crate::ui::components::button::{ButtonKind, button};
use crate::ui::components::hud::{
    self, HudButtonKind, divider, floating_pill, hud_button, skip_button, volume_icon,
};
use crate::ui::components::pick_dialog::pick_dialog;
use crate::ui::theme;
use crate::ui::view_models::{episode_meta, mode_label};

actions!(
    player,
    [
        TogglePause,
        ToggleFullscreen,
        Escape,
        SeekBack,
        SeekForward,
        VolumeUp,
        VolumeDown,
        NextEpisode,
        PreviousEpisode,
        ToggleMute
    ]
);

const CONTEXT: &str = "Player";
/// Idle delay after the last intentional pointer move before the HUD fades out.
const HIDE_AFTER: Duration = Duration::from_millis(1750);
/// How long the view's size must hold before mpv renders at it.
const RESIZE_SETTLE: Duration = Duration::from_millis(150);
const UP_NEXT_AFTER: Duration = Duration::from_secs(5);
const SEEK_STEP: f64 = 10.0;
const VOLUME_STEP: f64 = 5.0;
const MAX_VOLUME: f64 = 100.0;
/// Episode picker grid: columns, and rows visible before it scrolls.
const EPISODE_COLUMNS: usize = 5;
const EPISODE_ROWS_SHOWN: usize = 7;
/// Fast HUD fade-in and quick fade-out durations.
const FADE_IN: Duration = Duration::from_millis(120);
const FADE_OUT: Duration = Duration::from_millis(180);
/// Smooth fade-in for the loading screen artwork.
const BACKDROP_FADE_IN: Duration = Duration::from_millis(260);
/// Minimum pointer travel (logical pixels) to count as intentional movement.
const POINTER_JITTER_PX: f32 = 4.0;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("space", TogglePause, Some(CONTEXT)),
        KeyBinding::new("f", ToggleFullscreen, Some(CONTEXT)),
        KeyBinding::new("escape", Escape, Some(CONTEXT)),
        KeyBinding::new("left", SeekBack, Some(CONTEXT)),
        KeyBinding::new("right", SeekForward, Some(CONTEXT)),
        KeyBinding::new("up", VolumeUp, Some(CONTEXT)),
        KeyBinding::new("down", VolumeDown, Some(CONTEXT)),
        KeyBinding::new("n", NextEpisode, Some(CONTEXT)),
        KeyBinding::new("p", PreviousEpisode, Some(CONTEXT)),
        KeyBinding::new("m", ToggleMute, Some(CONTEXT)),
    ]);
}

enum Phase {
    /// Starting the player or asking ani-cli for the stream; `pick` holds
    /// ani-cli's titles when none of them matched the show.
    Resolving {
        pick: Option<Vec<String>>,
    },
    Playing,
    Failed {
        heading: SharedString,
        message: SharedString,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OverlayVisibility {
    Visible,
    FadingOut,
    Hidden,
}

/// Result of resolving an episode off the UI thread (including optional DB
/// resume lookup and local download check).
enum EpisodeLoad {
    Local {
        path: PathBuf,
        start: Option<f64>,
    },
    Stream {
        stream: Stream,
        from_cache: bool,
        start: Option<f64>,
    },
    NeedsPick {
        candidates: Vec<String>,
        start: Option<f64>,
    },
}

/// The countdown to autoplaying `ep` after the current episode ended.
struct UpNext {
    ep: String,
    _timer: Task<()>,
}

pub struct PlayerView {
    root: WeakEntity<Root>,
    req: PlayRequest,
    phase: Phase,
    player: Option<MpvPlayer>,
    /// A stream ready before `MpvPlayer::new` finished: `(stream, from_cache)`.
    pending_stream: Option<(Stream, bool)>,
    /// Whether the currently loaded stream came from the in-memory URL cache.
    loaded_from_cache: bool,
    /// Prevents infinite retry loops if a fresh resolve also fails in mpv.
    retried_after_cache: bool,
    /// Handed to the player's change callback once the player exists.
    wake: Option<UnboundedSender<()>>,
    db: Option<Arc<Mutex<Db>>>,
    downloads: Option<Arc<DownloadManager>>,
    cache_dir: PathBuf,
    shims: Result<PathBuf, String>,
    autoplay: bool,
    focus: FocusHandle,
    focused: bool,
    state: PlaybackState,
    /// `time_pos` has moved past 0 since this episode loaded.
    started: bool,
    /// Position at the last progress save.
    saved_at: f64,
    eof_handled: bool,
    up_next: Option<UpNext>,
    image: Option<Arc<RenderImage>>,
    /// Drops the current video frame on the next render (e.g. when switching to
    /// a cached/prefetched episode that transitions straight back to `Playing`).
    clear_frame: bool,
    /// AniList metadata for the show (episode thumbnails, banner, MAL id).
    media: Option<AniListMedia>,
    episode_title: Option<String>,
    /// Local cached artwork shown behind "Finding stream…" and "Loading video…".
    backdrop: Option<PathBuf>,
    backdrop_url: Option<String>,
    backdrop_shown: u64,
    media_task: Option<Task<()>>,
    backdrop_task: Option<Task<()>>,
    /// Skip timestamps (AniSkip + mpv chapters) and floating prompt state.
    aniskip: Vec<AniSkipEntry>,
    chapters: Vec<Chapter>,
    skip_times: SkipTimes,
    skip_task: Option<Task<()>>,
    prompt_tracker: PromptTracker,
    prompt_vis: OverlayVisibility,
    prompt_shown: Option<ActivePrompt>,
    prompt_gen: u64,
    prompt_hovered: bool,
    prompt_hide_task: Option<Task<()>>,
    prompt_fade_task: Option<Task<()>>,
    /// Device-pixel size mpv renders at, and a newer one still settling.
    render_size: Option<(u32, u32)>,
    pending_size: Option<(u32, u32)>,
    resize_task: Option<Task<()>>,
    /// HUD visibility state and fade animation generation.
    hud: OverlayVisibility,
    hud_gen: u64,
    hud_fade_task: Option<Task<()>>,
    /// Pointer is over interactive controls in the bottom bar or top bar.
    bottom_hovered: bool,
    top_hovered: bool,
    /// Last pointer position that counted as intentional movement (filters jitter).
    last_pointer: Option<(f32, f32)>,
    /// The episode picker is open.
    episodes_open: bool,
    episode_scroll: UniformListScrollHandle,
    /// Seek bar fraction under the pointer, for the time bubble.
    seek_hover: Option<f64>,
    hide_at: Instant,
    hide_task: Option<Task<()>>,
    seek_bounds: Rc<Cell<Bounds<Pixels>>>,
    volume_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// Seek bar fraction while it is being dragged.
    scrub: Option<f64>,
    volume_drag: bool,
    /// Volume to restore on unmute.
    unmute_volume: Option<f64>,
    create_task: Option<Task<()>>,
    resolve_task: Option<Task<()>>,
    /// Kills this view's foreground ani-cli runs when it goes away or switches episode.
    resolves: ResolveScope,
    /// Background pre-resolve for the next episode near the end of the current one.
    prefetch_ep: Option<String>,
    prefetch_task: Option<Task<bool>>,
    prefetch_scope: ResolveScope,
    leaving: bool,
    /// Holds off screen blanking while video actually plays.
    keep_awake: KeepAwake,
    _wake_task: Task<()>,
}

impl PlayerView {
    pub fn new(req: PlayRequest, root: WeakEntity<Root>, cx: &mut Context<Self>) -> Self {
        let (db, downloads, cache_dir, shims, autoplay) = match cx.try_global::<AppState>() {
            Some(s) => (
                Some(s.db.clone()),
                Some(s.downloads.clone()),
                s.cache_dir.clone(),
                s.shims.clone(),
                s.settings.autoplay_next,
            ),
            None => (
                None,
                None,
                paths::cache_dir(),
                Err("The library isn't open yet.".to_string()),
                true,
            ),
        };
        let (tx, mut rx) = mpsc::unbounded::<()>();
        let wake_task = cx.spawn(async move |this, cx| {
            while rx.next().await.is_some() {
                while rx.try_recv().is_ok() {}
                if this.update(cx, |view, cx| view.player_changed(cx)).is_err() {
                    break;
                }
            }
        });
        cx.on_release(Self::release).detach();
        let saved_at = req.start;
        let mut view = PlayerView {
            root,
            req,
            phase: Phase::Resolving { pick: None },
            player: None,
            pending_stream: None,
            loaded_from_cache: false,
            retried_after_cache: false,
            wake: Some(tx),
            db,
            downloads,
            cache_dir,
            shims,
            autoplay,
            focus: cx.focus_handle(),
            focused: false,
            state: PlaybackState::default(),
            started: false,
            saved_at,
            eof_handled: false,
            up_next: None,
            image: None,
            clear_frame: false,
            media: None,
            episode_title: None,
            backdrop: None,
            backdrop_url: None,
            backdrop_shown: 0,
            media_task: None,
            backdrop_task: None,
            aniskip: Vec::new(),
            chapters: Vec::new(),
            skip_times: SkipTimes::default(),
            skip_task: None,
            prompt_tracker: PromptTracker::default(),
            prompt_vis: OverlayVisibility::Hidden,
            prompt_shown: None,
            prompt_gen: 0,
            prompt_hovered: false,
            prompt_hide_task: None,
            prompt_fade_task: None,
            render_size: None,
            pending_size: None,
            resize_task: None,
            hud: OverlayVisibility::Visible,
            hud_gen: 0,
            hud_fade_task: None,
            bottom_hovered: false,
            top_hovered: false,
            last_pointer: None,
            episodes_open: false,
            episode_scroll: UniformListScrollHandle::new(),
            seek_hover: None,
            hide_at: Instant::now(),
            hide_task: None,
            seek_bounds: Rc::default(),
            volume_bounds: Rc::default(),
            scrub: None,
            volume_drag: false,
            unmute_volume: None,
            create_task: None,
            resolve_task: None,
            resolves: ResolveScope::default(),
            prefetch_ep: None,
            prefetch_task: None,
            prefetch_scope: ResolveScope::default(),
            leaving: false,
            keep_awake: KeepAwake::default(),
            _wake_task: wake_task,
        };
        // Start mpv initialization, show metadata/artwork, and stream resolution
        // in parallel so none waits serially on another.
        view.create_player(cx);
        view.load_backdrop(cx);
        view.load_media(cx);
        view.start_episode(cx);
        view
    }

    fn create_player(&mut self, cx: &mut Context<Self>) {
        let created = cx.background_spawn(async { MpvPlayer::new() });
        self.create_task = Some(cx.spawn(async move |this, cx| {
            let mut created = Some(created.await);
            this.update(cx, |view, cx| {
                if let Some(created) = created.take() {
                    view.player_created(created, cx);
                }
            })
            .ok();
            if let Some(Ok(player)) = created {
                discard(player);
            }
        }));
    }

    fn player_created(&mut self, created: Result<MpvPlayer, PlayerError>, cx: &mut Context<Self>) {
        match created {
            Ok(player) => {
                if let Some(tx) = self.wake.take() {
                    player.set_on_change(Box::new(move || {
                        let _ = tx.unbounded_send(());
                    }));
                }
                if let Some((w, h)) = self.render_size {
                    player.set_render_size(w, h);
                }
                self.player = Some(player);
                if let Some((stream, from_cache)) = self.pending_stream.take() {
                    self.load(stream, from_cache, cx);
                }
            }
            Err(PlayerError::LibraryMissing(_)) => {
                self.pending_stream = None;
                self.resolve_task = None;
                self.fail("Video playback needs libmpv", LIBMPV_INSTALL_HINT.into());
            }
            Err(err) => {
                self.pending_stream = None;
                self.resolve_task = None;
                self.fail("The video player couldn't start", err.to_string());
            }
        }
        cx.notify();
    }

    /// Loads the show's AniList metadata off the UI thread (SQLite first,
    /// falling back to `catalog::media` when only an AniList id is known) to
    /// supply episode thumbnails, show banner, and the MAL id for AniSkip.
    fn load_media(&mut self, cx: &mut Context<Self>) {
        let Some(db) = self.db.clone() else {
            return;
        };
        let show_id = self.req.show.id.clone();
        let anilist_id = self.req.show.anilist_id;
        let loaded = cx.background_spawn(async move {
            let record = {
                let db = db.lock().unwrap_or_else(PoisonError::into_inner);
                db.get_show(&show_id).ok().flatten()
            };
            if let Some(media) = record.as_ref().and_then(|r| r.media.clone()) {
                return Some(media);
            }
            let id = anilist_id.or_else(|| record.and_then(|r| r.summary.anilist_id))?;
            catalog::media(&db, id)
        });
        self.media_task = Some(cx.spawn(async move |this, cx| {
            let media = loaded.await;
            this.update(cx, |view, cx| {
                if view.leaving {
                    return;
                }
                if let Some(media) = media {
                    view.media = Some(media);
                    view.refresh_episode_meta(cx);
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Updates episode title, loading backdrop, and AniSkip timestamps for the
    /// current episode whenever `self.media` or `self.req.ep` changes.
    fn refresh_episode_meta(&mut self, cx: &mut Context<Self>) {
        self.episode_title = self
            .media
            .as_ref()
            .and_then(|m| episode_meta(m, &self.req.ep))
            .and_then(|(title, _)| title.map(str::to_string));
        self.load_backdrop(cx);
        self.load_skip_times(cx);
    }

    /// Downloads the highest-priority loading backdrop off the UI thread
    /// (episode thumbnail → show banner → AniList cover → AllAnime cover).
    fn load_backdrop(&mut self, cx: &mut Context<Self>) {
        let candidates = backdrop_candidates(self.media.as_ref(), &self.req.show, &self.req.ep);
        if candidates.is_empty() {
            return;
        }
        // If we are already displaying the top candidate, avoid re-fetching or re-fading.
        if self.backdrop.is_some() && self.backdrop_url.as_deref() == Some(candidates[0].as_str()) {
            return;
        }
        // Drop a previous episode's thumbnail while the new episode's artwork
        // loads, while keeping shared show-level banner/cover fallbacks.
        if self
            .backdrop_url
            .as_ref()
            .is_some_and(|cur| !candidates.contains(cur))
        {
            self.backdrop = None;
            self.backdrop_url = None;
        }
        let dir = self.cache_dir.join("images");
        let ep = self.req.ep.clone();
        let fetched = cx.background_spawn(async move {
            for url in candidates {
                if let Ok(path) = images::cached_image(&url, &dir) {
                    return Some((url, path));
                }
            }
            None
        });
        self.backdrop_task = Some(cx.spawn(async move |this, cx| {
            let result = fetched.await;
            this.update(cx, |view, cx| {
                if view.leaving || view.req.ep != ep {
                    return;
                }
                if let Some((url, path)) = result
                    && view.backdrop_url.as_deref() != Some(url.as_str())
                {
                    view.backdrop_url = Some(url);
                    view.backdrop = Some(path);
                    view.backdrop_shown += 1;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Fetches AniSkip opening/ending timestamps off the UI thread once the
    /// show's MAL id is known, cached per `(mal_id, ep)` in `api_cache`.
    fn load_skip_times(&mut self, cx: &mut Context<Self>) {
        self.aniskip.clear();
        self.recompute_skip_times(cx);
        let (Some(db), Some(mal_id)) =
            (self.db.clone(), self.media.as_ref().and_then(|m| m.id_mal))
        else {
            self.skip_task = None;
            return;
        };
        let ep = self.req.ep.clone();
        let ep_bg = ep.clone();
        let fetched =
            cx.background_spawn(async move { skip_times::fetch_aniskip(&db, mal_id, &ep_bg) });
        self.skip_task = Some(cx.spawn(async move |this, cx| {
            let entries = fetched.await;
            this.update(cx, |view, cx| {
                if view.leaving || view.req.ep != ep {
                    return;
                }
                view.aniskip = entries;
                view.recompute_skip_times(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    fn recompute_skip_times(&mut self, cx: &mut Context<Self>) {
        self.skip_times = skip_times::resolve(&self.aniskip, &self.chapters, self.state.duration);
        self.sync_prompt(cx);
    }

    /// Plays the request's file or stream; without either, checks downloads,
    /// cached/prefetched streams, and ani-cli in one off-UI-thread step.
    fn start_episode(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = &self.req.local_file {
            let stream = Stream {
                url: path.to_string_lossy().into_owned(),
                referrer: None,
                audio_file: None,
                sub_file: None,
                title: None,
            };
            self.load(stream, false, cx);
        } else if let Some(stream) = self.req.stream.take() {
            playback::cache_stream(
                &self.req.show.id,
                self.req.mode,
                &self.req.ep,
                &self.req.quality,
                stream.clone(),
            );
            self.load(stream, false, cx);
        } else {
            self.resolve_episode(None, false, None, cx);
        }
    }

    /// Resolves the current episode off the UI thread in one combined task:
    /// optional saved resume lookup → downloaded file → short-lived stream
    /// cache → in-flight prefetch (if any) → ani-cli.
    fn resolve_episode(
        &mut self,
        chosen: Option<String>,
        load_resume: bool,
        inflight_prefetch: Option<Task<bool>>,
        cx: &mut Context<Self>,
    ) {
        self.phase = Phase::Resolving { pick: None };
        let shims = self.shims.clone();
        let db = self.db.clone();
        let downloads = self.downloads.clone();
        let (show, ep) = (self.req.show.clone(), self.req.ep.clone());
        let (mode, quality) = (self.req.mode, self.req.quality.clone());
        let owner = self.resolves.handle();
        let resolved = cx.background_spawn(async move {
            let start = if load_resume {
                let saved = db
                    .as_ref()
                    .and_then(|db| {
                        let db = db.lock().unwrap_or_else(PoisonError::into_inner);
                        db.progress_for_show(&show.id, mode).ok()
                    })
                    .unwrap_or_default();
                Some(playback::resume_point(&saved, &ep))
            } else {
                None
            };
            if chosen.is_none() {
                let records: Vec<DownloadRecord> = downloads
                    .map(|d| d.snapshot().into_iter().map(|(record, _)| record).collect())
                    .unwrap_or_default();
                if let Some(path) = playback::downloaded_file(&records, &show.id, mode, &ep) {
                    return Ok(EpisodeLoad::Local { path, start });
                }
                if let Some(stream) = playback::play_cached(&show.id, mode, &ep, &quality) {
                    return Ok(EpisodeLoad::Stream {
                        stream,
                        from_cache: true,
                        start,
                    });
                }
                if let Some(prefetch) = inflight_prefetch
                    && prefetch.await
                    && let Some(stream) = playback::play_cached(&show.id, mode, &ep, &quality)
                {
                    return Ok(EpisodeLoad::Stream {
                        stream,
                        from_cache: true,
                        start,
                    });
                }
            }
            let shims = shims.map_err(|err| anyhow::anyhow!(err))?;
            let Some(db) = db else {
                anyhow::bail!("The library isn't open yet.");
            };
            match playback::resolve(
                &db,
                &shims,
                &show,
                &ep,
                mode,
                &quality,
                chosen.as_deref(),
                &owner,
            )? {
                ResolveOutcome::Stream(stream) => Ok(EpisodeLoad::Stream {
                    stream,
                    from_cache: false,
                    start,
                }),
                ResolveOutcome::NeedsPick(candidates) => {
                    Ok(EpisodeLoad::NeedsPick { candidates, start })
                }
            }
        });
        let expected_ep = self.req.ep.clone();
        self.resolve_task = Some(cx.spawn(async move |this, cx| {
            let outcome = resolved.await;
            this.update(cx, |view, cx| {
                if view.leaving || view.req.ep != expected_ep {
                    return;
                }
                match outcome {
                    Ok(EpisodeLoad::Local { path, start }) => {
                        if let Some(start) = start {
                            view.req.start = start;
                        }
                        view.req.local_file = Some(path.clone());
                        let stream = Stream {
                            url: path.to_string_lossy().into_owned(),
                            referrer: None,
                            audio_file: None,
                            sub_file: None,
                            title: None,
                        };
                        view.load(stream, false, cx);
                    }
                    Ok(EpisodeLoad::Stream {
                        stream,
                        from_cache,
                        start,
                    }) => {
                        if let Some(start) = start {
                            view.req.start = start;
                        }
                        view.load(stream, from_cache, cx);
                    }
                    Ok(EpisodeLoad::NeedsPick { candidates, start }) => {
                        if let Some(start) = start {
                            view.req.start = start;
                        }
                        view.phase = Phase::Resolving {
                            pick: Some(candidates),
                        };
                    }
                    Err(err) => view.fail(view.failed_heading(), format!("{err:#}")),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn choose(&mut self, title: String, cx: &mut Context<Self>) {
        if matches!(self.phase, Phase::Resolving { pick: Some(_) }) {
            self.resolve_episode(Some(title), false, None, cx);
        }
    }

    fn load(&mut self, stream: Stream, from_cache: bool, cx: &mut Context<Self>) {
        if self.leaving {
            return;
        }
        let Some(player) = self.player.as_ref() else {
            // `MpvPlayer::new` is still running in parallel; hand off as soon
            // as it finishes.
            self.pending_stream = Some((stream, from_cache));
            return;
        };
        self.started = false;
        self.clear_frame = true;
        self.eof_handled = false;
        self.saved_at = self.req.start;
        self.up_next = None;
        self.loaded_from_cache = from_cache;
        self.chapters.clear();
        self.prompt_tracker.reset();
        self.prompt_hide_task = None;
        self.prompt_fade_task = None;
        self.prompt_vis = OverlayVisibility::Hidden;
        self.prompt_shown = None;
        self.prompt_hovered = false;
        match player.load(&stream, self.req.start) {
            Ok(()) => {
                self.phase = Phase::Playing;
                self.recompute_skip_times(cx);
                self.poke(cx);
            }
            Err(err) => self.fail(self.failed_heading(), err.to_string()),
        }
    }

    fn failed_heading(&self) -> SharedString {
        format!("Couldn't play episode {}", self.req.ep).into()
    }

    fn fail(&mut self, heading: impl Into<SharedString>, message: String) {
        if let Some(player) = &self.player {
            player.stop();
        }
        self.up_next = None;
        self.cancel_prefetch();
        self.prompt_tracker.reset();
        self.prompt_hide_task = None;
        self.prompt_fade_task = None;
        self.prompt_vis = OverlayVisibility::Hidden;
        self.prompt_shown = None;
        self.phase = Phase::Failed {
            heading: heading.into(),
            message: message.into(),
        };
    }

    /// Runs on the UI thread after the player reported new frames or state.
    fn player_changed(&mut self, cx: &mut Context<Self>) {
        if self.leaving || !matches!(self.phase, Phase::Playing) {
            return;
        }
        let Some(player) = &self.player else {
            return;
        };
        if let Some(err) = player.last_error() {
            // If a cached/prefetched stream URL expired before playback even
            // started, drop it from cache and resolve a fresh URL once.
            if !self.started && self.loaded_from_cache && !self.retried_after_cache {
                self.retried_after_cache = true;
                self.loaded_from_cache = false;
                playback::evict_stream(
                    &self.req.show.id,
                    self.req.mode,
                    &self.req.ep,
                    &self.req.quality,
                );
                player.stop();
                self.resolve_episode(None, false, None, cx);
                cx.notify();
                return;
            }
            self.save(cx).detach();
            self.fail(self.failed_heading(), err);
            cx.notify();
            return;
        }
        let prev = std::mem::replace(&mut self.state, player.state());
        let now = self.state;
        let chapters = player.chapters();
        if !self.started && now.time_pos > 0.0 {
            self.started = true;
            self.saved_at = now.time_pos;
        }
        if chapters != self.chapters || (now.duration - prev.duration).abs() > 0.5 {
            self.chapters = chapters;
            self.recompute_skip_times(cx);
        }
        let can_queue = self.autoplay && self.next_ep().is_some();
        let d = playback::decide(
            &prev,
            &now,
            self.started,
            self.saved_at,
            self.eof_handled,
            can_queue,
        );
        self.eof_handled = d.eof_handled;
        if d.save {
            self.save(cx).detach();
        }
        if d.queue_next {
            self.queue_next(cx);
        }
        if d.cancel_up_next {
            self.up_next = None;
        }
        if now.paused != prev.paused {
            self.poke(cx);
        }
        self.sync_prompt(cx);
        // Near the end of the episode, silently pre-resolve the next one so
        // advancing has zero stream-finding wait (never writes ani-cli history).
        if let Some(next) = self.next_ep()
            && self.prefetch_ep.as_deref() != Some(next.as_str())
            && playback::should_prefetch_next(
                &self.state,
                self.started,
                self.skip_times.outro.map(|s| s.start),
            )
        {
            self.start_prefetch(next, cx);
        }
        cx.notify();
    }

    fn start_prefetch(&mut self, ep: String, cx: &mut Context<Self>) {
        let Ok(shims) = self.shims.clone() else {
            return;
        };
        let Some(db) = self.db.clone() else {
            return;
        };
        self.cancel_prefetch();
        self.prefetch_ep = Some(ep.clone());
        let downloads = self.downloads.clone();
        let show = self.req.show.clone();
        let (mode, quality) = (self.req.mode, self.req.quality.clone());
        let owner = self.prefetch_scope.handle();
        self.prefetch_task = Some(cx.background_spawn(async move {
            let records: Vec<DownloadRecord> = downloads
                .map(|d| d.snapshot().into_iter().map(|(record, _)| record).collect())
                .unwrap_or_default();
            if playback::downloaded_file(&records, &show.id, mode, &ep).is_some() {
                return false;
            }
            playback::resolve_ahead(&db, &shims, &show, &ep, mode, &quality, &owner)
                .inspect_err(|err| eprintln!("ani-gui: finding episode {ep} ahead: {err:#}"))
                .unwrap_or(false)
        }));
    }

    fn cancel_prefetch(&mut self) {
        self.prefetch_task = None;
        self.prefetch_ep = None;
        self.prefetch_scope = ResolveScope::default();
    }

    /// The current episode's progress, once playback has started.
    fn snapshot(&self) -> Option<Progress> {
        if !self.started {
            return None;
        }
        Some(Progress {
            show_id: self.req.show.id.clone(),
            mode: self.req.mode,
            ep: self.req.ep.clone(),
            position: playback::saved_position(&self.state),
            duration: self.state.duration,
            updated_at: db::now(),
        })
    }

    fn save(&mut self, cx: &mut Context<Self>) -> Task<()> {
        let (Some(db), Some(progress)) = (self.db.clone(), self.snapshot()) else {
            return Task::ready(());
        };
        self.saved_at = self.state.time_pos;
        cx.background_spawn(async move { save_progress(&db, &progress) })
    }

    fn next_ep(&self) -> Option<String> {
        playback::next_episode(&self.req.episodes, &self.req.ep)
    }

    fn queue_next(&mut self, cx: &mut Context<Self>) {
        let Some(ep) = self.next_ep() else {
            return;
        };
        let timer = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(UP_NEXT_AFTER).await;
            this.update(cx, |view, cx| {
                if let Some(next) = view.up_next.take() {
                    view.advance(next.ep, cx);
                }
            })
            .ok();
        });
        self.up_next = Some(UpNext { ep, _timer: timer });
        self.sync_prompt(cx);
    }

    fn prev_ep(&self) -> Option<String> {
        previous_episode(&self.req.episodes, &self.req.ep)
    }

    fn play_next(&mut self, cx: &mut Context<Self>) {
        if self.leaving || !matches!(self.phase, Phase::Playing) {
            return;
        }
        if let Some(ep) = self.next_ep() {
            self.advance(ep, cx);
        }
    }

    fn play_previous(&mut self, cx: &mut Context<Self>) {
        if self.leaving || !matches!(self.phase, Phase::Playing) {
            return;
        }
        if let Some(ep) = self.prev_ep() {
            self.advance(ep, cx);
        }
    }

    fn skip_intro(&mut self, cx: &mut Context<Self>) {
        let Some(intro) = self.skip_times.intro else {
            return;
        };
        if let Some(player) = self.playing() {
            player.seek_absolute(intro.end);
            self.state.time_pos = intro.end;
            self.sync_prompt(cx);
            cx.notify();
        }
    }

    fn toggle_episodes(&mut self, cx: &mut Context<Self>) {
        self.episodes_open = !self.episodes_open && self.req.episodes.len() > 1;
        if self.episodes_open
            && let Some(i) = self.req.episodes.iter().position(|e| *e == self.req.ep)
        {
            self.episode_scroll
                .scroll_to_item(i / EPISODE_COLUMNS, ScrollStrategy::Center);
        }
        self.sync_prompt(cx);
        self.poke(cx);
        cx.notify();
    }

    /// Inhibits screen blanking exactly while video is playing: loaded,
    /// started, not paused, not at the end, not failed or leaving.
    fn sync_keep_awake(&mut self) {
        let playing = !self.leaving
            && matches!(self.phase, Phase::Playing)
            && self.player.is_some()
            && self.started
            && !self.state.paused
            && !self.state.eof;
        self.keep_awake.set_active(playing);
    }

    /// Saves the current episode, then plays `ep` in the same quality: from
    /// its saved position unless finished, and from its download or cached/
    /// prefetched stream when available.
    fn advance(&mut self, ep: String, cx: &mut Context<Self>) {
        if self.leaving {
            return;
        }
        self.save(cx).detach();
        self.up_next = None;
        self.resolves = ResolveScope::default();
        let adopt_prefetch = if self.prefetch_ep.as_deref() == Some(ep.as_str()) {
            self.prefetch_ep = None;
            self.prefetch_task.take()
        } else {
            self.cancel_prefetch();
            None
        };
        if let Some(player) = &self.player {
            player.stop();
        }
        self.pending_stream = None;
        self.loaded_from_cache = false;
        self.retried_after_cache = false;
        self.state = PlaybackState::default();
        self.started = false;
        self.clear_frame = true;
        self.scrub = None;
        self.chapters.clear();
        self.prompt_tracker.reset();
        self.prompt_hide_task = None;
        self.prompt_fade_task = None;
        self.prompt_vis = OverlayVisibility::Hidden;
        self.prompt_shown = None;
        self.prompt_hovered = false;
        self.req.ep = ep;
        self.req.start = 0.0;
        self.req.stream = None;
        self.req.local_file = None;
        self.refresh_episode_meta(cx);
        self.resolve_episode(None, true, adopt_prefetch, cx);
    }

    /// Pauses, saves progress, then returns to the previous page.
    pub fn leave(&mut self, cx: &mut Context<Self>) {
        if self.leaving {
            return;
        }
        self.leaving = true;
        self.up_next = None;
        self.pending_stream = None;
        self.resolve_task = None;
        self.resolves = ResolveScope::default();
        self.cancel_prefetch();
        self.sync_keep_awake();
        if let Some(player) = &self.player {
            player.set_pause(true);
        }
        let saved = self.save(cx);
        let root = self.root.clone();
        let id = cx.entity_id();
        cx.spawn(async move |_, cx| {
            saved.await;
            root.update(cx, |root, cx| root.close_player(id, cx)).ok();
        })
        .detach();
    }

    /// Synchronously saves the latest playback position before the app exits,
    /// reusing the player's normal progress snapshot/write path.
    pub fn save_for_quit(&mut self) {
        self.keep_awake.set_active(false);
        if let Some(player) = &self.player {
            let now = player.state();
            if now.time_pos > 0.0 {
                self.started = true;
            }
            self.state = now;
        }
        if let (Some(progress), Some(db)) = (self.snapshot(), &self.db) {
            save_progress(db, &progress);
            self.saved_at = self.state.time_pos;
        }
        self.leaving = true;
    }

    /// The view is gone: frees the last frame's texture, saves progress
    /// right here (the app may be quitting, and one upsert is quick) and
    /// tears the player down off the UI thread (mpv waits for its network
    /// threads).
    fn release(&mut self, cx: &mut App) {
        self.keep_awake.set_active(false);
        self.cancel_prefetch();
        self.resolves = ResolveScope::default();
        if let Some(image) = self.image.take() {
            cx.defer(move |cx| cx.drop_image(image, None));
        }
        if !self.leaving {
            self.save_for_quit();
        }
        if let Some(player) = self.player.take() {
            discard(player);
        }
    }

    // Controls.

    fn playing(&self) -> Option<&MpvPlayer> {
        match self.phase {
            Phase::Playing => self.player.as_ref(),
            _ => None,
        }
    }

    fn toggle_pause(&mut self, cx: &mut Context<Self>) {
        if let Some(player) = self.playing() {
            player.toggle_pause();
        }
        self.poke(cx);
    }

    fn seek_by(&mut self, secs: f64, cx: &mut Context<Self>) {
        if let Some(player) = self.playing() {
            player.seek_relative(secs);
        }
        self.poke(cx);
    }

    fn set_volume(&mut self, volume: f64, cx: &mut Context<Self>) {
        let volume = volume.clamp(0.0, MAX_VOLUME);
        if let Some(player) = self.playing() {
            player.set_volume(volume);
            self.unmute_volume = None;
            self.state.volume = volume;
        }
        self.poke(cx);
    }

    fn toggle_mute(&mut self, cx: &mut Context<Self>) {
        if self.playing().is_none() {
            return;
        }
        let volume = match self.unmute_volume.take() {
            Some(volume) => volume,
            None => {
                self.unmute_volume = Some(self.state.volume);
                0.0
            }
        };
        if let Some(player) = self.playing() {
            player.set_volume(volume);
        }
        self.state.volume = volume;
        self.poke(cx);
    }

    fn toggle_subs(&mut self, cx: &mut Context<Self>) {
        if let Some(player) = self.playing() {
            player.toggle_subs();
        }
        self.poke(cx);
    }

    fn escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.episodes_open {
            self.episodes_open = false;
            self.sync_prompt(cx);
            self.poke(cx);
            cx.notify();
        } else if window.is_fullscreen() {
            window.toggle_fullscreen();
        } else {
            self.leave(cx);
        }
    }

    fn go_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.is_fullscreen() {
            window.toggle_fullscreen();
        }
        self.leave(cx);
    }

    /// Shows the controls and restarts their idle timer.
    fn poke(&mut self, cx: &mut Context<Self>) {
        self.hide_at = Instant::now() + HIDE_AFTER;
        self.hud_fade_task = None;
        if self.hud != OverlayVisibility::Visible {
            self.hud = OverlayVisibility::Visible;
            self.hud_gen += 1;
            self.sync_prompt(cx);
            cx.notify();
        }
        if self.hide_task.is_some() {
            return;
        }
        self.hide_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok(wait) = this.read_with(cx, |view, _| {
                    view.hide_at.saturating_duration_since(Instant::now())
                }) else {
                    return;
                };
                if wait.is_zero() {
                    break;
                }
                cx.background_executor().timer(wait).await;
            }
            this.update(cx, |view, cx| {
                view.hide_task = None;
                if view.may_hide_controls() {
                    view.start_hud_fade_out(cx);
                }
            })
            .ok();
        }));
    }

    fn start_hud_fade_out(&mut self, cx: &mut Context<Self>) {
        if self.hud != OverlayVisibility::Visible {
            return;
        }
        self.hud = OverlayVisibility::FadingOut;
        self.hud_gen += 1;
        self.seek_hover = None;
        self.sync_prompt(cx);
        self.hud_fade_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FADE_OUT).await;
            this.update(cx, |view, cx| {
                view.hud_fade_task = None;
                if view.hud == OverlayVisibility::FadingOut {
                    view.hud = OverlayVisibility::Hidden;
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn may_hide_controls(&self) -> bool {
        matches!(self.phase, Phase::Playing)
            && !self.state.paused
            && self.scrub.is_none()
            && !self.volume_drag
            && !self.bottom_hovered
            && !self.top_hovered
            && !self.episodes_open
    }

    /// Reconciles the floating "Skip intro" / "Next episode" button with the
    /// playback position, HUD visibility, hover state, and 8 s auto-hide grace.
    fn sync_prompt(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        let eligible = if !self.leaving && matches!(self.phase, Phase::Playing) && self.started {
            skip_times::eligible_prompt(
                &self.skip_times,
                self.state.time_pos,
                self.state.duration,
                self.next_ep().is_some(),
                self.up_next.is_some(),
            )
        } else {
            None
        };
        if self.prompt_tracker.update(eligible, now) {
            self.prompt_hide_task = None;
            if let Some(wait) = self.prompt_tracker.time_until_hide(now) {
                self.schedule_prompt_hide(wait, cx);
            }
        }
        let want = self.prompt_tracker.visible(
            now,
            self.hud == OverlayVisibility::Visible,
            self.prompt_hovered,
            self.episodes_open || self.up_next.is_some(),
        );
        match (self.prompt_vis, want) {
            (_, Some(kind)) => {
                if self.prompt_vis != OverlayVisibility::Visible || self.prompt_shown != Some(kind)
                {
                    self.prompt_fade_task = None;
                    self.prompt_vis = OverlayVisibility::Visible;
                    self.prompt_shown = Some(kind);
                    self.prompt_gen += 1;
                }
            }
            (OverlayVisibility::Visible, None) => {
                self.prompt_vis = OverlayVisibility::FadingOut;
                self.prompt_gen += 1;
                self.prompt_hovered = false;
                self.prompt_fade_task = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(FADE_OUT).await;
                    this.update(cx, |view, cx| {
                        view.prompt_fade_task = None;
                        if view.prompt_vis == OverlayVisibility::FadingOut {
                            view.prompt_vis = OverlayVisibility::Hidden;
                            view.prompt_shown = None;
                            cx.notify();
                        }
                    })
                    .ok();
                }));
            }
            (OverlayVisibility::FadingOut | OverlayVisibility::Hidden, None) => {}
        }
    }

    fn schedule_prompt_hide(&mut self, wait: Duration, cx: &mut Context<Self>) {
        self.prompt_hide_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            this.update(cx, |view, cx| {
                view.prompt_hide_task = None;
                view.sync_prompt(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    fn seek_down(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        if self.playing().is_some() && self.state.duration > 0.0 {
            self.scrub = Some(fraction_at(self.seek_bounds.get(), event.position.x));
            self.poke(cx);
        }
    }

    fn volume_down(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        self.volume_drag = true;
        let f = fraction_at(self.volume_bounds.get(), event.position.x);
        self.set_volume(f * MAX_VOLUME, cx);
    }

    fn mouse_moved(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let pos = (f32::from(event.position.x), f32::from(event.position.y));
        let dragging = self.scrub.is_some() || self.volume_drag;
        if dragging || pointer_moved_enough(self.last_pointer, pos) {
            self.last_pointer = Some(pos);
            self.poke(cx);
        } else if self.hud != OverlayVisibility::Visible {
            // Sub-pixel / tiny mouse jitter while hidden must not pop the HUD back up.
            return;
        }
        let bounds = self.seek_bounds.get();
        let hover = (self.hud == OverlayVisibility::Visible && bounds.contains(&event.position))
            .then(|| fraction_at(bounds, event.position.x));
        if hover != self.seek_hover {
            self.seek_hover = hover;
            cx.notify();
        }
        let held = event.pressed_button == Some(MouseButton::Left);
        if self.scrub.is_some() {
            if held {
                self.scrub = Some(fraction_at(self.seek_bounds.get(), event.position.x));
                cx.notify();
            } else {
                self.commit_scrub(cx);
            }
        }
        if self.volume_drag {
            if held {
                let f = fraction_at(self.volume_bounds.get(), event.position.x);
                self.set_volume(f * MAX_VOLUME, cx);
            } else {
                self.volume_drag = false;
                self.poke(cx);
            }
        }
    }

    fn mouse_released(&mut self, cx: &mut Context<Self>) {
        let was_dragging = self.volume_drag || self.scrub.is_some();
        self.volume_drag = false;
        self.commit_scrub(cx);
        if was_dragging {
            self.poke(cx);
        }
    }

    fn commit_scrub(&mut self, cx: &mut Context<Self>) {
        let Some(f) = self.scrub.take() else {
            return;
        };
        if let Some(player) = self.playing() {
            let target = f * self.state.duration;
            player.seek_absolute(target);
            self.state.time_pos = target;
            self.sync_prompt(cx);
        }
        cx.notify();
    }

    /// The view measured `size` device pixels: the first size applies at
    /// once, later ones after they settle.
    fn measured(&mut self, size: (u32, u32), cx: &mut Context<Self>) {
        if self.render_size.is_none() {
            self.apply_size(size);
            return;
        }
        if self.pending_size.or(self.render_size) == Some(size) {
            return;
        }
        self.pending_size = Some(size);
        self.resize_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RESIZE_SETTLE).await;
            this.update(cx, |view, _| {
                if let Some(size) = view.pending_size.take() {
                    view.apply_size(size);
                }
            })
            .ok();
        }));
    }

    fn apply_size(&mut self, (w, h): (u32, u32)) {
        self.render_size = Some((w, h));
        if let Some(player) = &self.player {
            player.set_render_size(w, h);
        }
    }

    /// Swaps in the player's newest frame, retiring the previous texture on
    /// the next frame so it is never removed from GPUI's Blade atlas before
    /// the current frame flushes pending texture initializations.
    fn sync_frame(&mut self, window: &mut Window) {
        if self.clear_frame {
            self.clear_frame = false;
            if let Some(old) = self.image.take() {
                retire_image(old, window);
            }
        }
        let Some(player) = self.playing() else {
            if let Some(old) = self.image.take() {
                retire_image(old, window);
            }
            return;
        };
        let Some(frame) = player.take_frame() else {
            return;
        };
        // gpui's images hold BGRA, which is what mpv renders.
        let Some(pixels) = RgbaImage::from_raw(frame.width, frame.height, frame.data) else {
            return;
        };
        let image = Arc::new(RenderImage::new(vec![image::Frame::new(pixels)]));
        if let Some(old) = self.image.replace(image) {
            retire_image(old, window);
        }
    }

    // Rendering.

    fn render_playing(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let known = self.pending_size.or(self.render_size);
        let view = cx.entity().downgrade();
        let measure = canvas(
            move |bounds, window, cx| {
                let scale = window.scale_factor();
                let size = (
                    (f32::from(bounds.size.width) * scale).round() as u32,
                    (f32::from(bounds.size.height) * scale).round() as u32,
                );
                if size.0 > 0 && size.1 > 0 && known != Some(size) {
                    cx.defer(move |cx| {
                        view.update(cx, |view, cx| view.measured(size, cx)).ok();
                    });
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0();
        let loading_first_frame = self.image.is_none();
        let rebuffering = self.image.is_some() && self.state.buffering;
        let hud_vis = self.hud;
        div()
            .size_full()
            .relative()
            .child(
                div()
                    .id("video")
                    .absolute()
                    .inset_0()
                    .children(
                        self.image
                            .clone()
                            .map(|image| img(image).size_full().object_fit(ObjectFit::Contain)),
                    )
                    .child(measure)
                    .on_click(cx.listener(|view, _, _, cx| {
                        // A click beside the open picker only closes it.
                        if view.episodes_open {
                            view.episodes_open = false;
                            view.sync_prompt(cx);
                            view.poke(cx);
                            cx.notify();
                        } else {
                            view.toggle_pause(cx);
                        }
                    })),
            )
            .when(loading_first_frame, |d| {
                d.child(self.render_loading_overlay("Loading video…"))
            })
            .when(rebuffering, |d| {
                d.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap(px(14.))
                        .child(searching_bar())
                        .child(
                            div()
                                .text_size(px(theme::SMALL))
                                .text_color(hud::ash())
                                .child("Buffering…"),
                        ),
                )
            })
            .when(hud_vis != OverlayVisibility::Hidden, |d| {
                let fading_out = hud_vis == OverlayVisibility::FadingOut;
                let anim = Animation::new(if fading_out { FADE_OUT } else { FADE_IN });
                d.child(
                    div()
                        .absolute()
                        .inset_0()
                        .child(self.render_top_bar(cx))
                        .child(self.render_bottom_bar(window, cx))
                        .with_animation(
                            ElementId::NamedInteger("player-controls".into(), self.hud_gen),
                            anim,
                            move |d, t| d.opacity(if fading_out { 1.0 - t } else { t }),
                        ),
                )
            })
            .children(self.render_prompt(cx))
            .children(
                self.up_next
                    .as_ref()
                    .map(|next| self.render_up_next(next, cx)),
            )
            .into_any_element()
    }

    /// Darkened episode thumbnail (or show banner / cover) faded in behind the
    /// loading status text so it stays readable across bright artwork.
    fn render_loading_backdrop(&self) -> Option<AnyElement> {
        let path = self.backdrop.clone()?;
        Some(
            div()
                .absolute()
                .inset_0()
                .child(img(path).size_full().object_fit(ObjectFit::Cover))
                .child(div().absolute().inset_0().bg(shade(0.56)))
                .child(div().absolute().inset_0().bg(linear_gradient(
                    180.,
                    linear_color_stop(shade(0.52), 0.),
                    linear_color_stop(shade(0.18), 0.45),
                )))
                .child(div().absolute().inset_0().bg(linear_gradient(
                    0.,
                    linear_color_stop(shade(0.76), 0.),
                    linear_color_stop(shade(0.16), 0.55),
                )))
                .with_animation(
                    ElementId::NamedInteger("player-backdrop".into(), self.backdrop_shown),
                    Animation::new(BACKDROP_FADE_IN).with_easing(ease_in_out),
                    |d, t| d.opacity(t),
                )
                .into_any_element(),
        )
    }

    /// Shared loading surface for "Finding stream…" and "Loading video…" so the
    /// artwork and title stay anchored across stages without jumping.
    fn render_loading_overlay(&self, status: &'static str) -> AnyElement {
        let ep_line = match &self.episode_title {
            Some(title) => format!("Episode {} · {title}", self.req.ep),
            None => format!("Episode {}", self.req.ep),
        };
        div()
            .absolute()
            .inset_0()
            .bg(theme::night())
            .children(self.render_loading_backdrop())
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(10.))
                    .px(px(32.))
                    .child(
                        div()
                            .font_family(theme::DISPLAY_FONT)
                            .font_weight(FontWeight::BOLD)
                            .text_size(px(theme::PAGE_TITLE))
                            .line_height(relative(1.))
                            .max_w(px(900.))
                            .text_center()
                            .text_color(hud::paper())
                            .child(self.req.show.display_title().to_string()),
                    )
                    .child(
                        div()
                            .text_size(px(theme::BODY))
                            .font_weight(FontWeight::MEDIUM)
                            .max_w(px(760.))
                            .text_center()
                            .text_color(hud::paper().opacity(0.84))
                            .child(ep_line),
                    )
                    .child(div().h(px(18.)))
                    .child(searching_bar())
                    .child(
                        div()
                            .text_size(px(theme::SMALL))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(hud::ash())
                            .child(status),
                    ),
            )
            .into_any_element()
    }

    /// Floating Netflix-style "Skip intro" / "Next episode" action pill, shown
    /// even while the rest of the HUD is hidden and faded out after ~8 s if
    /// untouched.
    fn render_prompt(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.prompt_vis == OverlayVisibility::Hidden {
            return None;
        }
        let kind = self.prompt_shown?;
        let fading_out = self.prompt_vis == OverlayVisibility::FadingOut;
        let pill = match kind {
            ActivePrompt::SkipIntro => {
                let tip: SharedString = match self.skip_times.intro {
                    Some(seg) => format!("Skip to {}", clock(seg.end)).into(),
                    None => "Skip opening".into(),
                };
                floating_pill("player-skip-intro", "skip-forward", "Skip intro", false)
                    .tooltip(move |_, cx| hud::tooltip(tip.clone(), cx))
                    .on_click(cx.listener(|view, _, _, cx| view.skip_intro(cx)))
            }
            ActivePrompt::NextEpisode => {
                let label = match self.next_ep() {
                    Some(ep) => format!("Next episode · E{ep}"),
                    None => "Next episode".into(),
                };
                floating_pill("player-next-ep-prompt", "play", label, true)
                    .tooltip(|_, cx| hud::tooltip("Play next episode (N)".into(), cx))
                    .on_click(cx.listener(|view, _, _, cx| view.play_next(cx)))
            }
        };
        let anim = Animation::new(if fading_out { FADE_OUT } else { FADE_IN });
        Some(
            div()
                .id("player-prompt-wrap")
                .absolute()
                .right(px(28.))
                .bottom(px(136.))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_hover(cx.listener(|view, hovered: &bool, _, cx| {
                    view.prompt_hovered = *hovered;
                    view.sync_prompt(cx);
                    cx.notify();
                }))
                .child(pill)
                .with_animation(
                    ElementId::NamedInteger("player-prompt".into(), self.prompt_gen),
                    anim,
                    move |d, t| d.opacity(if fading_out { 1.0 - t } else { t }),
                )
                .into_any_element(),
        )
    }

    /// What is playing and where: the bottom-left of the controls.
    fn render_title(&self, position: f64, duration: f64) -> Div {
        div()
            .flex()
            .flex_col()
            .gap(px(3.))
            .min_w_0()
            .child(
                div()
                    .font_family(theme::DISPLAY_FONT)
                    .font_weight(FontWeight::BOLD)
                    .text_size(px(26.))
                    .line_height(relative(1.05))
                    .text_color(hud::paper())
                    .truncate()
                    .child(self.req.show.display_title().to_string()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .text_size(px(theme::SMALL))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(hud::ash())
                    .child(
                        div()
                            .text_color(hud::paper())
                            .child(format!("Episode {}", self.req.ep)),
                    )
                    .child(
                        div()
                            .px(px(7.))
                            .rounded(px(5.))
                            .border_1()
                            .border_color(hud::hairline())
                            .text_size(px(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(mode_label(self.req.mode)),
                    )
                    .child(if duration > 0.0 {
                        format!("{} / {}", clock(position), clock(duration))
                    } else {
                        clock(position)
                    }),
            )
    }

    fn render_top_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(px(150.))
            .flex()
            .items_start()
            .px(px(28.))
            .pt(px(22.))
            .bg(linear_gradient(
                180.,
                linear_color_stop(shade(0.66), 0.),
                linear_color_stop(shade(0.), 1.),
            ))
            .child(
                div()
                    .id("player-top-controls")
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_hover(cx.listener(|view, hovered: &bool, _, cx| {
                        view.top_hovered = *hovered;
                        if !*hovered {
                            view.poke(cx);
                        }
                    }))
                    .child(back_button(cx)),
            )
            .into_any_element()
    }

    fn render_bottom_bar(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let s = self.state;
        let duration = s.duration.max(0.0);
        let position = match self.scrub {
            Some(f) => f * duration,
            None => s.time_pos.clamp(0.0, duration.max(s.time_pos)),
        };
        let fraction = if duration > 0.0 {
            (position / duration) as f32
        } else {
            0.0
        };
        let next = self.next_ep();
        let prev = self.prev_ep();
        let seek_bounds = self.seek_bounds.clone();
        let volume_bounds = self.volume_bounds.clone();

        // The time under the pointer, floating over the seek bar.
        let bubble = self
            .scrub
            .or(self.seek_hover)
            .filter(|_| duration > 0.0)
            .map(|f| {
                div()
                    .absolute()
                    .bottom(px(24.))
                    .left(relative(f as f32))
                    .ml(px(-30.))
                    .w(px(60.))
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .px(px(8.))
                            .py(px(3.))
                            .rounded(px(6.))
                            .bg(theme::night_alpha(0.94))
                            .border_1()
                            .border_color(hud::hairline())
                            .text_size(px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(hud::paper())
                            .child(clock(f * duration)),
                    )
            });
        let seek = div()
            .id("seek")
            .group("seek")
            .relative()
            .w_full()
            .h(px(22.))
            .flex()
            .items_center()
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, e: &MouseDownEvent, _, cx| view.seek_down(e, cx)),
            )
            .child(hud::seek_track(fraction, "seek", self.scrub.is_some()))
            .children(bubble)
            .child(record_bounds(seek_bounds));

        let volume = div()
            .id("volume")
            .group("volume")
            .relative()
            .w(px(88.))
            .h(px(36.))
            .flex()
            .items_center()
            .cursor_pointer()
            .tooltip(|_, cx| hud::tooltip("Volume (↑ ↓)".into(), cx))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, e: &MouseDownEvent, _, cx| view.volume_down(e, cx)),
            )
            .child(hud::volume_track(
                (s.volume / MAX_VOLUME) as f32,
                "volume",
                self.volume_drag,
            ))
            .child(record_bounds(volume_bounds));

        let transport = div()
            .flex()
            .items_center()
            .gap(px(14.))
            .child(
                hud_button(
                    "player-prev",
                    "skip-back",
                    match &prev {
                        Some(ep) => format!("Episode {ep} (P)"),
                        None => "No previous episode".into(),
                    },
                )
                .kind(HudButtonKind::Glass)
                .disabled(prev.is_none())
                .on_click(cx.listener(|view, _, _, cx| view.play_previous(cx))),
            )
            .child(
                skip_button("player-back-10", false, SEEK_STEP as u32, "Back 10 s (←)")
                    .on_click(cx.listener(|view, _, _, cx| view.seek_by(-SEEK_STEP, cx))),
            )
            .child(
                hud_button(
                    "player-play",
                    if s.paused { "play" } else { "pause" },
                    if s.paused {
                        "Play (Space)"
                    } else {
                        "Pause (Space)"
                    },
                )
                .kind(HudButtonKind::Play)
                .on_click(cx.listener(|view, _, _, cx| view.toggle_pause(cx))),
            )
            .child(
                skip_button(
                    "player-forward-10",
                    true,
                    SEEK_STEP as u32,
                    "Forward 10 s (→)",
                )
                .on_click(cx.listener(|view, _, _, cx| view.seek_by(SEEK_STEP, cx))),
            )
            .child(
                hud_button(
                    "player-next",
                    "skip-forward",
                    match &next {
                        Some(ep) => format!("Episode {ep} (N)"),
                        None => "No next episode".into(),
                    },
                )
                .kind(HudButtonKind::Glass)
                .disabled(next.is_none())
                .on_click(cx.listener(|view, _, _, cx| view.play_next(cx))),
            );

        let fullscreen = window.is_fullscreen();
        let has_episodes = self.req.episodes.len() > 1;
        let utilities = div()
            .flex()
            .items_center()
            .justify_end()
            .gap(px(2.))
            .child(
                hud_button(
                    "player-mute",
                    volume_icon(s.volume),
                    if s.volume <= 0.0 {
                        "Unmute (M)"
                    } else {
                        "Mute (M)"
                    },
                )
                .on_click(cx.listener(|view, _, _, cx| view.toggle_mute(cx))),
            )
            .child(volume)
            .child(divider())
            .child(
                hud_button(
                    "player-subs",
                    if s.sub_visible {
                        "captions"
                    } else {
                        "captions-off"
                    },
                    if s.sub_visible {
                        "Hide subtitles"
                    } else {
                        "Show subtitles"
                    },
                )
                .on_click(cx.listener(|view, _, _, cx| view.toggle_subs(cx))),
            )
            .child(
                div()
                    .relative()
                    .child(
                        hud_button("player-episodes", "list-video", "Episodes")
                            .disabled(!has_episodes)
                            .active(self.episodes_open)
                            .on_click(cx.listener(|view, _, _, cx| view.toggle_episodes(cx))),
                    )
                    .when(self.episodes_open && has_episodes, |d| {
                        d.child(deferred(self.render_episodes(cx)).with_priority(1))
                    }),
            )
            .child(
                hud_button(
                    "player-fullscreen",
                    if fullscreen { "minimize" } else { "maximize" },
                    if fullscreen {
                        "Exit full screen (F)"
                    } else {
                        "Full screen (F)"
                    },
                )
                .on_click(cx.listener(|view, _, window, cx| {
                    window.toggle_fullscreen();
                    view.poke(cx);
                })),
            );

        div()
            .id("player-hud")
            .absolute()
            .bottom_0()
            .left_0()
            .right_0()
            .flex()
            .flex_col()
            .pt(px(96.))
            .bg(linear_gradient(
                0.,
                linear_color_stop(shade(0.9), 0.),
                linear_color_stop(shade(0.), 1.),
            ))
            .child(
                div()
                    .id("player-bottom-controls")
                    .flex()
                    .flex_col()
                    .px(px(28.))
                    .pb(px(20.))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_hover(cx.listener(|view, hovered: &bool, _, cx| {
                        view.bottom_hovered = *hovered;
                        if !*hovered {
                            view.poke(cx);
                        }
                    }))
                    .child(seek)
                    .child(
                        // Title at the start, the transport on the centre line,
                        // settings at the end.
                        div()
                            .mt(px(10.))
                            .flex()
                            .items_center()
                            .gap(px(24.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(self.render_title(position, duration)),
                            )
                            .child(transport)
                            .child(div().flex_1().flex().justify_end().child(utilities)),
                    ),
            )
            .into_any_element()
    }

    /// The show's episodes as a grid of numbers; the current one in the
    /// accent, any other a click away.
    fn render_episodes(&self, cx: &mut Context<Self>) -> AnyElement {
        let episodes = self.req.episodes.clone();
        let rows = episodes.len().div_ceil(EPISODE_COLUMNS);
        let current = self.req.ep.clone();
        let list = uniform_list(
            "player-episode-rows",
            rows,
            cx.processor(move |_, range: std::ops::Range<usize>, _, cx| {
                let mut rows = Vec::with_capacity(range.len());
                for row in range {
                    let start = row * EPISODE_COLUMNS;
                    let end = (start + EPISODE_COLUMNS).min(episodes.len());
                    let mut line = div().flex().gap(px(6.)).pb(px(6.));
                    for ep in &episodes[start..end] {
                        line = line.child(episode_chip(ep, *ep == current, cx));
                    }
                    rows.push(line);
                }
                rows
            }),
        )
        .track_scroll(self.episode_scroll.clone())
        .h(px((rows.min(EPISODE_ROWS_SHOWN) * 42) as f32));
        div()
            .id("player-episodes-panel")
            .absolute()
            .bottom(px(54.))
            .right(px(-60.))
            .w(px(EPISODE_COLUMNS as f32 * 50. + 22.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(14.))
            .pr(px(8.))
            .rounded(px(14.))
            .bg(theme::night_alpha(0.95))
            .border_1()
            .border_color(hud::hairline())
            .shadow_lg()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .justify_between()
                    .pr(px(6.))
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(hud::paper())
                            .child("Episodes"),
                    )
                    .child(
                        div()
                            .text_size(px(theme::SMALL))
                            .text_color(hud::ash())
                            .child(self.req.episodes.len().to_string()),
                    ),
            )
            .child(list)
            .into_any_element()
    }

    fn render_up_next(&self, next: &UpNext, cx: &mut Context<Self>) -> AnyElement {
        let bar_id = ElementId::Name(format!("up-next-{}", next.ep).into());
        div()
            .id("up-next")
            .absolute()
            .right(px(28.))
            .bottom(px(140.))
            .w(px(300.))
            .flex()
            .flex_col()
            .rounded(px(14.))
            .overflow_hidden()
            .bg(theme::night_alpha(0.94))
            .border_1()
            .border_color(theme::line())
            .shadow_lg()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .p(px(20.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(px(theme::SMALL))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme::text_dim())
                                    .child("Up next"),
                            )
                            .child(
                                div()
                                    .font_family(theme::DISPLAY_FONT)
                                    .font_weight(FontWeight::BOLD)
                                    .text_size(px(44.))
                                    .line_height(relative(1.))
                                    .child(format!("Episode {}", next.ep)),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(button("up-next-play", "Play now").compact().on_click(
                                cx.listener(|view, _, _, cx| {
                                    if let Some(next) = view.up_next.take() {
                                        view.advance(next.ep, cx);
                                    }
                                }),
                            ))
                            .child(
                                button("up-next-cancel", "Cancel")
                                    .kind(ButtonKind::Ghost)
                                    .compact()
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.up_next = None;
                                        view.sync_prompt(cx);
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .child(div().h(px(3.)).bg(gpui::white().opacity(0.08)).child(
                div().h_full().bg(theme::accent()).with_animation(
                    bar_id,
                    Animation::new(UP_NEXT_AFTER),
                    |bar, t| bar.w(relative(1. - t)),
                ),
            ))
            .into_any_element()
    }

    fn render_resolving(&self, pick: Option<&[String]>, cx: &mut Context<Self>) -> AnyElement {
        let status = if self.player.is_none() && self.pending_stream.is_some() {
            "Starting the player…"
        } else {
            "Finding stream…"
        };
        div()
            .size_full()
            .relative()
            .child(self.render_loading_overlay(status))
            .child(
                div()
                    .absolute()
                    .top(px(22.))
                    .left(px(28.))
                    .child(back_button(cx)),
            )
            .children(pick.map(|candidates| self.render_pick(candidates, cx)))
            .into_any_element()
    }

    fn render_pick(&self, candidates: &[String], cx: &mut Context<Self>) -> AnyElement {
        pick_dialog(
            self.req.show.display_title(),
            candidates,
            cx.listener(|view, title: &str, _, cx| view.choose(title.to_string(), cx)),
            cx.listener(|view, _, window, cx| view.go_back(window, cx)),
            None,
        )
    }

    fn render_failed(
        &self,
        heading: &SharedString,
        message: &SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_start()
            .justify_center()
            .gap(px(14.))
            .px(theme::gutter())
            .bg(theme::night())
            .child(
                div()
                    .font_family(theme::DISPLAY_FONT)
                    .font_weight(FontWeight::BOLD)
                    .text_size(px(theme::PAGE_TITLE))
                    .line_height(relative(1.))
                    .child(heading.clone()),
            )
            .child(
                div()
                    .max_w(px(680.))
                    .text_size(px(theme::BODY))
                    .line_height(relative(1.5))
                    .text_color(theme::text_dim())
                    .child(message.clone()),
            )
            .child(
                div().pt(px(10.)).child(
                    button("player-failed-back", "Back")
                        .kind(ButtonKind::Secondary)
                        .on_click(cx.listener(|view, _, window, cx| view.go_back(window, cx))),
                ),
            )
            .into_any_element()
    }
}

impl Render for PlayerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused {
            window.focus(&self.focus);
            self.focused = true;
        }
        self.sync_frame(window);
        self.sync_keep_awake();
        let body = match &self.phase {
            Phase::Playing => self.render_playing(window, cx),
            Phase::Resolving { pick } => self.render_resolving(pick.as_deref(), cx),
            Phase::Failed { heading, message } => self.render_failed(heading, message, cx),
        };
        let hide_cursor = self.hud != OverlayVisibility::Visible
            && !self.prompt_hovered
            && matches!(self.phase, Phase::Playing);
        div()
            .id("player")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|view, _: &TogglePause, _, cx| view.toggle_pause(cx)))
            .on_action(cx.listener(|view, _: &ToggleFullscreen, window, cx| {
                window.toggle_fullscreen();
                view.poke(cx);
            }))
            .on_action(cx.listener(|view, _: &Escape, window, cx| view.escape(window, cx)))
            .on_action(cx.listener(|view, _: &Back, window, cx| view.go_back(window, cx)))
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Back),
                cx.listener(|view, _, window, cx| {
                    cx.stop_propagation();
                    view.go_back(window, cx);
                }),
            )
            .on_action(cx.listener(|view, _: &SeekBack, _, cx| view.seek_by(-SEEK_STEP, cx)))
            .on_action(cx.listener(|view, _: &SeekForward, _, cx| view.seek_by(SEEK_STEP, cx)))
            .on_action(cx.listener(|view, _: &VolumeUp, _, cx| {
                view.set_volume(view.state.volume + VOLUME_STEP, cx)
            }))
            .on_action(cx.listener(|view, _: &VolumeDown, _, cx| {
                view.set_volume(view.state.volume - VOLUME_STEP, cx)
            }))
            .on_action(cx.listener(|view, _: &NextEpisode, _, cx| view.play_next(cx)))
            .on_action(cx.listener(|view, _: &PreviousEpisode, _, cx| view.play_previous(cx)))
            .on_action(cx.listener(|view, _: &ToggleMute, _, cx| view.toggle_mute(cx)))
            .on_mouse_move(cx.listener(|view, e: &MouseMoveEvent, _, cx| view.mouse_moved(e, cx)))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|view, _, _, cx| view.mouse_released(cx)),
            )
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(gpui::black())
            .text_color(theme::text())
            .when(hide_cursor, |d| d.cursor(CursorStyle::None))
            .child(body)
    }
}

/// Drops a retired video frame on the next frame so it is never removed from
/// GPUI's Blade atlas while the current frame still has pending texture
/// initializations for it.
fn retire_image(image: Arc<RenderImage>, window: &mut Window) {
    window.on_next_frame(move |window, _| {
        let _ = window.drop_image(image);
    });
}

/// Loading-screen image URLs in priority order: episode thumbnail, show
/// banner, AniList cover, AllAnime cover — deduplicated and non-empty.
fn backdrop_candidates(media: Option<&AniListMedia>, show: &ShowSummary, ep: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut push = |url: &str| {
        if !url.is_empty() && !urls.iter().any(|existing: &String| existing == url) {
            urls.push(url.to_string());
        }
    };
    if let Some(media) = media {
        if let Some((_, thumb)) = episode_meta(media, ep) {
            push(thumb);
        }
        if let Some(banner) = &media.banner_url {
            push(banner);
        }
        push(&media.cover_url);
    }
    push(&show.cover_url);
    urls
}

/// True when the pointer moved far enough from `prev` to count as intentional
/// movement rather than sub-pixel / sensor jitter.
fn pointer_moved_enough(prev: Option<(f32, f32)>, (x, y): (f32, f32)) -> bool {
    match prev {
        None => true,
        Some((px, py)) => {
            let dx = x - px;
            let dy = y - py;
            dx * dx + dy * dy >= POINTER_JITTER_PX * POINTER_JITTER_PX
        }
    }
}

/// Tears `player` down on its own thread.
fn discard(player: MpvPlayer) {
    let spawned = thread::Builder::new()
        .name("player-teardown".into())
        .spawn(move || drop(player));
    if let Err(err) = spawned {
        eprintln!("ani-gui: tearing the player down: {err}");
    }
}

fn save_progress(db: &Mutex<Db>, progress: &Progress) {
    crate::lists::progress_saved(progress);
    let db = db.lock().unwrap_or_else(PoisonError::into_inner);
    if let Err(err) = db.save_progress(progress) {
        eprintln!("ani-gui: saving progress: {err:#}");
    }
}

/// The episode before `ep` in `episodes` (ascending), matching labels like
/// `progress::next_after` does: exactly, else by number.
fn previous_episode(episodes: &[String], ep: &str) -> Option<String> {
    let num = |e: &str| e.parse::<f64>().ok().filter(|n| n.is_finite());
    let target = num(ep);
    let same = |e: &str| e == ep || (target.is_some() && num(e) == target);
    match episodes.iter().position(|e| same(e)) {
        Some(i) => i.checked_sub(1).and_then(|i| episodes.get(i)).cloned(),
        None => {
            let target = target?;
            episodes
                .iter()
                .rev()
                .find(|e| num(e).is_some_and(|n| n < target))
                .cloned()
        }
    }
}

/// `m:ss`, or `h:mm:ss` from an hour up.
fn clock(secs: f64) -> String {
    let total = if secs.is_finite() && secs > 0.0 {
        secs as u64
    } else {
        0
    };
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn fraction_at(bounds: Bounds<Pixels>, x: Pixels) -> f64 {
    let width = f32::from(bounds.size.width);
    if width <= 0.0 {
        return 0.0;
    }
    (f32::from(x - bounds.origin.x) / width).clamp(0.0, 1.0) as f64
}

/// Black at `alpha`, for the scrims over video.
fn shade(alpha: f32) -> Hsla {
    gpui::black().opacity(alpha)
}

/// Fills its parent and records its bounds each frame, for pointer math.
fn record_bounds(cell: Rc<Cell<Bounds<Pixels>>>) -> impl IntoElement {
    canvas(move |bounds, _, _| cell.set(bounds), |_, _, _, _| {})
        .absolute()
        .inset_0()
}

/// The glass arrow that leaves the player.
fn back_button(cx: &mut Context<PlayerView>) -> impl IntoElement {
    hud_button("player-back", "arrow-left", "Back (Esc)")
        .kind(HudButtonKind::Glass)
        .on_click(cx.listener(|view, _, window, cx| view.go_back(window, cx)))
}

/// One episode in the picker.
fn episode_chip(ep: &str, current: bool, cx: &mut Context<PlayerView>) -> impl IntoElement {
    let target = ep.to_string();
    div()
        .id(ElementId::Name(format!("player-ep-{ep}").into()))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .w(px(44.))
        .h(px(36.))
        .rounded(px(8.))
        .text_size(px(14.))
        .font_weight(FontWeight::SEMIBOLD)
        .tooltip({
            let label: SharedString = format!("Episode {ep}").into();
            move |_, cx| hud::tooltip(label.clone(), cx)
        })
        .when(current, |d| {
            d.bg(theme::accent()).text_color(theme::on_accent())
        })
        .when(!current, |d| {
            d.bg(gpui::white().opacity(0.06))
                .text_color(hud::paper())
                .cursor_pointer()
                .hover(|s| s.bg(gpui::white().opacity(0.16)))
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.episodes_open = false;
                    view.advance(target.clone(), cx);
                }))
        })
        .child(ep.to_string())
}

/// An accent sliver sweeping a thin track while something loads.
fn searching_bar() -> impl IntoElement {
    div()
        .relative()
        .w(px(220.))
        .h(px(3.))
        .rounded(px(2.))
        .overflow_hidden()
        .bg(gpui::white().opacity(0.16))
        .child(
            div()
                .absolute()
                .top_0()
                .h_full()
                .w(relative(0.3))
                .rounded(px(2.))
                .bg(theme::accent())
                .with_animation(
                    "searching",
                    Animation::new(Duration::from_millis(1300))
                        .repeat()
                        .with_easing(ease_in_out),
                    |bar, t| bar.left(relative(t * 1.3 - 0.3)),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_formats_minutes_and_hours() {
        assert_eq!(clock(0.0), "0:00");
        assert_eq!(clock(65.9), "1:05");
        assert_eq!(clock(1440.0), "24:00");
        assert_eq!(clock(3725.0), "1:02:05");
        assert_eq!(clock(-3.0), "0:00");
        assert_eq!(clock(f64::NAN), "0:00");
    }

    #[test]
    fn previous_episode_steps_back_through_the_list() {
        let eps: Vec<String> = ["1", "2", "3", "3.5", "4"]
            .iter()
            .map(|e| e.to_string())
            .collect();
        assert_eq!(previous_episode(&eps, "1"), None);
        assert_eq!(previous_episode(&eps, "2").as_deref(), Some("1"));
        assert_eq!(previous_episode(&eps, "4").as_deref(), Some("3.5"));
        assert_eq!(previous_episode(&eps, "04").as_deref(), Some("3.5"));
        // Not in the list: the nearest lower number.
        assert_eq!(previous_episode(&eps, "3.2").as_deref(), Some("3"));
        assert_eq!(previous_episode(&eps, "special"), None);
        assert_eq!(previous_episode(&[], "2"), None);
    }

    #[test]
    fn backdrop_candidates_prioritizes_episode_thumb_then_banner_then_cover() {
        let show = ShowSummary {
            id: "s1".into(),
            title: "Frieren".into(),
            english_title: None,
            cover_url: "https://img/show-cover.jpg".into(),
            anilist_id: Some(154587),
            sub_count: 28,
            dub_count: 0,
        };
        let media = AniListMedia {
            id: 154587,
            id_mal: Some(52991),
            title_romaji: "Sousou no Frieren".into(),
            cover_url: "https://img/al-cover.jpg".into(),
            banner_url: Some("https://img/banner.jpg".into()),
            episode_thumbs: vec![
                (
                    "Episode 1 - The Journey's End".into(),
                    "https://img/ep1.jpg".into(),
                ),
                (
                    "Episode 2 - It Didn't Have to Be Magic".into(),
                    "https://img/ep2.jpg".into(),
                ),
            ],
            ..Default::default()
        };
        assert_eq!(
            backdrop_candidates(Some(&media), &show, "2"),
            vec![
                "https://img/ep2.jpg",
                "https://img/banner.jpg",
                "https://img/al-cover.jpg",
                "https://img/show-cover.jpg",
            ]
        );
        // Episode without thumbnail falls back to banner → covers.
        assert_eq!(
            backdrop_candidates(Some(&media), &show, "99"),
            vec![
                "https://img/banner.jpg",
                "https://img/al-cover.jpg",
                "https://img/show-cover.jpg",
            ]
        );
        // No media falls back to show cover.
        assert_eq!(
            backdrop_candidates(None, &show, "1"),
            vec!["https://img/show-cover.jpg"]
        );
    }

    #[test]
    fn pointer_moved_enough_filters_tiny_jitter() {
        assert!(pointer_moved_enough(None, (100.0, 100.0)));
        assert!(!pointer_moved_enough(Some((100.0, 100.0)), (100.5, 100.5)));
        assert!(!pointer_moved_enough(Some((100.0, 100.0)), (103.0, 102.0)));
        assert!(pointer_moved_enough(Some((100.0, 100.0)), (104.0, 100.0)));
        assert!(pointer_moved_enough(Some((100.0, 100.0)), (103.0, 103.0)));
    }
}
