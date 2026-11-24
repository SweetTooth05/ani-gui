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
    NavigationDirection, ObjectFit, PathBuilder, Pixels, RenderImage, SharedString, Stateful, Task,
    WeakEntity, Window, actions, canvas, div, ease_in_out, img, linear_color_stop, linear_gradient,
    point, prelude::*, px, relative,
};
use image::RgbaImage;

use crate::anicli::{ResolveOutcome, ResolveScope};
use crate::app::{AppState, Back, PlayRequest, Root};
use crate::db::{self, Db};
use crate::downloads::DownloadManager;
use crate::model::{DownloadRecord, Progress, Stream};
use crate::playback;
use crate::player::{MpvPlayer, PlaybackState, PlayerError};
use crate::ui::components::button::{ButtonKind, button};
use crate::ui::components::pick_dialog::pick_dialog;
use crate::ui::theme;
use crate::ui::view_models::mode_label;

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
        ToggleMute
    ]
);

const CONTEXT: &str = "Player";
const HIDE_AFTER: Duration = Duration::from_secs(3);
/// How long the view's size must hold before mpv renders at it.
const RESIZE_SETTLE: Duration = Duration::from_millis(150);
const UP_NEXT_AFTER: Duration = Duration::from_secs(5);
const SEEK_STEP: f64 = 10.0;
const SKIP_INTRO: f64 = 85.0;
const VOLUME_STEP: f64 = 5.0;
const MAX_VOLUME: f64 = 100.0;
const LIBMPV_MISSING: &str = "Install libmpv2 (sudo apt install libmpv2)";

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
    /// Handed to the player's change callback once the player exists.
    wake: Option<UnboundedSender<()>>,
    db: Option<Arc<Mutex<Db>>>,
    downloads: Option<Arc<DownloadManager>>,
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
    /// Device-pixel size mpv renders at, and a newer one still settling.
    render_size: Option<(u32, u32)>,
    pending_size: Option<(u32, u32)>,
    resize_task: Option<Task<()>>,
    controls: bool,
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
    /// Kills this view's ani-cli runs when it goes away.
    resolves: ResolveScope,
    leaving: bool,
    _wake_task: Task<()>,
}

impl PlayerView {
    pub fn new(req: PlayRequest, root: WeakEntity<Root>, cx: &mut Context<Self>) -> Self {
        let (db, downloads, shims, autoplay) = match cx.try_global::<AppState>() {
            Some(s) => (
                Some(s.db.clone()),
                Some(s.downloads.clone()),
                s.shims.clone(),
                s.settings.autoplay_next,
            ),
            None => (
                None,
                None,
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
            wake: Some(tx),
            db,
            downloads,
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
            render_size: None,
            pending_size: None,
            resize_task: None,
            controls: true,
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
            leaving: false,
            _wake_task: wake_task,
        };
        view.create_player(cx);
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
                self.start_episode(cx);
            }
            Err(PlayerError::LibraryMissing(_)) => {
                self.fail("Video playback needs libmpv", LIBMPV_MISSING.into());
            }
            Err(err) => self.fail("The video player couldn't start", err.to_string()),
        }
        cx.notify();
    }

    /// Plays the request's file or stream; without either, the episode's
    /// download if there is one, else what ani-cli resolves.
    fn start_episode(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = &self.req.local_file {
            let stream = Stream {
                url: path.to_string_lossy().into_owned(),
                referrer: None,
                audio_file: None,
                sub_file: None,
                title: None,
            };
            self.load(stream, cx);
        } else if let Some(stream) = self.req.stream.take() {
            self.load(stream, cx);
        } else {
            self.find_download(cx);
        }
    }

    /// Looks for the current episode's download off the UI thread, then
    /// plays it or resolves the stream.
    fn find_download(&mut self, cx: &mut Context<Self>) {
        self.phase = Phase::Resolving { pick: None };
        let downloads = self.downloads.clone();
        let (show_id, mode, ep) = (self.req.show.id.clone(), self.req.mode, self.req.ep.clone());
        let found = cx.background_spawn(async move {
            let records: Vec<DownloadRecord> = downloads
                .map(|d| d.snapshot().into_iter().map(|(record, _)| record).collect())
                .unwrap_or_default();
            playback::downloaded_file(&records, &show_id, mode, &ep)
        });
        self.resolve_task = Some(cx.spawn(async move |this, cx| {
            let file = found.await;
            this.update(cx, |view, cx| {
                if view.leaving {
                    return;
                }
                match file {
                    Some(file) => {
                        view.req.local_file = Some(file);
                        view.start_episode(cx);
                    }
                    None => view.resolve(None, cx),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Asks ani-cli for the current episode's stream. With `chosen` (a title
    /// the user picked from ani-cli's results), searches for that title and
    /// remembers it for the show.
    fn resolve(&mut self, chosen: Option<String>, cx: &mut Context<Self>) {
        let shims = match &self.shims {
            Ok(shims) => shims.clone(),
            Err(err) => {
                let err = err.clone();
                self.fail(self.failed_heading(), err);
                return;
            }
        };
        let Some(db) = self.db.clone() else {
            return;
        };
        self.phase = Phase::Resolving { pick: None };
        let (show, ep) = (self.req.show.clone(), self.req.ep.clone());
        let (mode, quality) = (self.req.mode, self.req.quality.clone());
        let owner = self.resolves.handle();
        let resolved = cx.background_spawn(async move {
            playback::resolve(
                &db,
                &shims,
                &show,
                &ep,
                mode,
                &quality,
                chosen.as_deref(),
                &owner,
            )
        });
        self.resolve_task = Some(cx.spawn(async move |this, cx| {
            let outcome = resolved.await;
            this.update(cx, |view, cx| {
                match outcome {
                    Ok(ResolveOutcome::Stream(stream)) => view.load(stream, cx),
                    Ok(ResolveOutcome::NeedsPick(candidates)) => {
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
            self.resolve(Some(title), cx);
        }
    }

    fn load(&mut self, stream: Stream, cx: &mut Context<Self>) {
        let Some(player) = self.player.as_ref().filter(|_| !self.leaving) else {
            return;
        };
        self.started = false;
        self.eof_handled = false;
        self.saved_at = self.req.start;
        self.up_next = None;
        match player.load(&stream, self.req.start) {
            Ok(()) => {
                self.phase = Phase::Playing;
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
            self.save(cx).detach();
            self.fail(self.failed_heading(), err);
            cx.notify();
            return;
        }
        let prev = std::mem::replace(&mut self.state, player.state());
        let now = self.state;
        if !self.started && now.time_pos > 0.0 {
            self.started = true;
            self.saved_at = now.time_pos;
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
        cx.notify();
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
    }

    fn play_next(&mut self, cx: &mut Context<Self>) {
        if self.leaving || !matches!(self.phase, Phase::Playing) {
            return;
        }
        if let Some(ep) = self.next_ep() {
            self.advance(ep, cx);
        }
    }

    /// Saves the current episode, then plays `ep` in the same quality: from
    /// its saved position unless finished, and from its download if there
    /// is one (otherwise ani-cli finds it and records it in its history; see
    /// `start_episode`).
    fn advance(&mut self, ep: String, cx: &mut Context<Self>) {
        if self.leaving {
            return;
        }
        self.save(cx).detach();
        self.up_next = None;
        if let Some(player) = &self.player {
            player.stop();
        }
        self.state = PlaybackState::default();
        self.started = false;
        self.scrub = None;
        self.req.ep = ep;
        self.req.start = 0.0;
        self.req.stream = None;
        self.req.local_file = None;
        self.phase = Phase::Resolving { pick: None };
        let db = self.db.clone();
        let (show_id, mode, ep) = (self.req.show.id.clone(), self.req.mode, self.req.ep.clone());
        let found = cx.background_spawn(async move {
            let saved = db
                .and_then(|db| {
                    let db = db.lock().unwrap_or_else(PoisonError::into_inner);
                    db.progress_for_show(&show_id, mode).ok()
                })
                .unwrap_or_default();
            playback::resume_point(&saved, &ep)
        });
        self.resolve_task = Some(cx.spawn(async move |this, cx| {
            let start = found.await;
            this.update(cx, |view, cx| {
                if view.leaving {
                    return;
                }
                view.req.start = start;
                view.start_episode(cx);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Pauses, saves progress, then returns to the previous page.
    pub fn leave(&mut self, cx: &mut Context<Self>) {
        if self.leaving {
            return;
        }
        self.leaving = true;
        self.up_next = None;
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

    /// The view is gone: frees the last frame's texture, saves progress
    /// right here (the app may be quitting, and one upsert is quick) and
    /// tears the player down off the UI thread (mpv waits for its network
    /// threads).
    fn release(&mut self, cx: &mut App) {
        if let Some(image) = self.image.take() {
            cx.defer(move |cx| cx.drop_image(image, None));
        }
        let last = if self.leaving { None } else { self.snapshot() };
        if let (Some(progress), Some(db)) = (last, &self.db) {
            save_progress(db, &progress);
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
        if window.is_fullscreen() {
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
        if !self.controls {
            self.controls = true;
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
                    view.controls = false;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn may_hide_controls(&self) -> bool {
        matches!(self.phase, Phase::Playing)
            && !self.state.paused
            && self.scrub.is_none()
            && !self.volume_drag
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
        self.poke(cx);
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
            }
        }
    }

    fn mouse_released(&mut self, cx: &mut Context<Self>) {
        self.volume_drag = false;
        self.commit_scrub(cx);
    }

    fn commit_scrub(&mut self, cx: &mut Context<Self>) {
        let Some(f) = self.scrub.take() else {
            return;
        };
        if let Some(player) = self.playing() {
            let target = f * self.state.duration;
            player.seek_absolute(target);
            self.state.time_pos = target;
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

    /// Swaps in the player's newest frame, freeing the previous texture.
    fn sync_frame(&mut self, window: &mut Window) {
        let Some(player) = self.playing() else {
            if let Some(old) = self.image.take() {
                let _ = window.drop_image(old);
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
            let _ = window.drop_image(old);
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
        let waiting = if self.state.buffering {
            Some("Buffering…")
        } else if self.image.is_none() {
            Some("Loading video…")
        } else {
            None
        };
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
                    .on_click(cx.listener(|view, _, _, cx| view.toggle_pause(cx))),
            )
            .children(waiting.map(|label| {
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
                            .text_color(theme::text_dim())
                            .child(label),
                    )
            }))
            .when(self.controls, |d| {
                d.child(self.render_top_bar(cx))
                    .child(self.render_bottom_bar(window, cx))
            })
            .children(
                self.up_next
                    .as_ref()
                    .map(|next| self.render_up_next(next, cx)),
            )
            .into_any_element()
    }

    fn render_title(&self) -> Div {
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .min_w_0()
            .child(
                div()
                    .font_family(theme::DISPLAY_FONT)
                    .font_weight(FontWeight::BOLD)
                    .text_size(px(30.))
                    .line_height(relative(1.05))
                    .truncate()
                    .child(self.req.show.display_title().to_string()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .text_size(px(theme::BODY))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme::text_dim())
                    .child(format!("Episode {}", self.req.ep))
                    .child(
                        div()
                            .px(px(8.))
                            .py(px(1.))
                            .rounded(px(5.))
                            .border_1()
                            .border_color(gpui::white().opacity(0.24))
                            .text_size(px(12.))
                            .child(mode_label(self.req.mode)),
                    ),
            )
    }

    fn render_top_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .flex()
            .items_start()
            .gap(px(18.))
            .px(px(28.))
            .pt(px(22.))
            .pb(px(56.))
            .bg(linear_gradient(
                180.,
                linear_color_stop(shade(0.82), 0.),
                linear_color_stop(shade(0.), 1.),
            ))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                control("player-back", "‹ Back")
                    .on_click(cx.listener(|view, _, window, cx| view.go_back(window, cx))),
            )
            .child(self.render_title())
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
        let has_next = self.next_ep().is_some();
        let muted = s.volume <= 0.0;
        let seek_bounds = self.seek_bounds.clone();
        let volume_bounds = self.volume_bounds.clone();

        let seek = div()
            .id("seek")
            .group("seek")
            .relative()
            .flex_1()
            .h(px(22.))
            .flex()
            .items_center()
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, e: &MouseDownEvent, _, cx| view.seek_down(e, cx)),
            )
            .child(track(fraction, px(4.), Some("seek"), self.scrub.is_some()))
            .child(record_bounds(seek_bounds));
        let volume = div()
            .id("volume")
            .group("volume")
            .relative()
            .w(px(92.))
            .h(px(22.))
            .flex()
            .items_center()
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, e: &MouseDownEvent, _, cx| view.volume_down(e, cx)),
            )
            .child(track(
                (s.volume / MAX_VOLUME) as f32,
                px(3.),
                Some("volume"),
                self.volume_drag,
            ))
            .child(record_bounds(volume_bounds));

        div()
            .absolute()
            .bottom_0()
            .left_0()
            .right_0()
            .flex()
            .flex_col()
            .gap(px(10.))
            .px(px(28.))
            .pt(px(72.))
            .pb(px(22.))
            .bg(linear_gradient(
                0.,
                linear_color_stop(shade(0.9), 0.),
                linear_color_stop(shade(0.), 1.),
            ))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(16.))
                    .text_size(px(theme::SMALL))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme::text_dim())
                    .child(div().min_w(px(52.)).child(clock(position)))
                    .child(seek)
                    .child(
                        div()
                            .min_w(px(52.))
                            .flex()
                            .justify_end()
                            .child(format!("−{}", clock(duration - position))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(play_button(s.paused, cx))
                    .child(div().w(px(8.)))
                    .child(
                        control("player-back-10", "−10 s")
                            .on_click(cx.listener(|view, _, _, cx| view.seek_by(-SEEK_STEP, cx))),
                    )
                    .child(
                        control("player-forward-10", "+10 s")
                            .on_click(cx.listener(|view, _, _, cx| view.seek_by(SEEK_STEP, cx))),
                    )
                    .child(
                        control("player-skip-intro", "Skip intro")
                            .on_click(cx.listener(|view, _, _, cx| view.seek_by(SKIP_INTRO, cx))),
                    )
                    .child(div().flex_1())
                    .child(if has_next {
                        control("player-next", "Next episode")
                            .on_click(cx.listener(|view, _, _, cx| view.play_next(cx)))
                    } else {
                        control_label("player-next", "Next episode").opacity(0.4)
                    })
                    .child(
                        control(
                            "player-subs",
                            if s.sub_visible {
                                "Hide subtitles"
                            } else {
                                "Show subtitles"
                            },
                        )
                        .on_click(cx.listener(|view, _, _, cx| view.toggle_subs(cx))),
                    )
                    .child(
                        control("player-mute", if muted { "Unmute" } else { "Mute" })
                            .on_click(cx.listener(|view, _, _, cx| view.toggle_mute(cx))),
                    )
                    .child(volume)
                    .child(div().w(px(6.)))
                    .child(
                        control(
                            "player-fullscreen",
                            if window.is_fullscreen() {
                                "Exit full screen"
                            } else {
                                "Full screen"
                            },
                        )
                        .on_click(cx.listener(|view, _, window, cx| {
                            window.toggle_fullscreen();
                            view.poke(cx);
                        })),
                    ),
            )
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
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(10.))
            .child(
                div()
                    .font_family(theme::DISPLAY_FONT)
                    .font_weight(FontWeight::BOLD)
                    .text_size(px(theme::PAGE_TITLE))
                    .line_height(relative(1.))
                    .max_w(px(900.))
                    .text_center()
                    .child(self.req.show.display_title().to_string()),
            )
            .child(
                div()
                    .text_size(px(theme::BODY))
                    .text_color(theme::text_dim())
                    .child(format!("Episode {}", self.req.ep)),
            )
            .child(div().h(px(18.)))
            .child(searching_bar())
            .child(
                div()
                    .text_size(px(theme::SMALL))
                    .text_color(theme::text_muted())
                    .child(if self.player.is_none() {
                        "Starting the player…"
                    } else {
                        "Finding stream…"
                    }),
            )
            .child(
                div().absolute().top(px(22.)).left(px(28.)).child(
                    control("player-back", "‹ Back")
                        .on_click(cx.listener(|view, _, window, cx| view.go_back(window, cx))),
                ),
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
        let body = match &self.phase {
            Phase::Playing => self.render_playing(window, cx),
            Phase::Resolving { pick } => self.render_resolving(pick.as_deref(), cx),
            Phase::Failed { heading, message } => self.render_failed(heading, message, cx),
        };
        let hide_cursor = !self.controls && matches!(self.phase, Phase::Playing);
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

/// A thin bar filled to `fraction` in the accent, thicker with a knob while
/// its `group` is hovered or `active`.
fn track(fraction: f32, height: Pixels, group: Option<&'static str>, active: bool) -> Div {
    let fraction = fraction.clamp(0.0, 1.0);
    let knob = div()
        .absolute()
        .top((height - px(12.)) / 2.)
        .left(relative(fraction))
        .ml(px(-6.))
        .size(px(12.))
        .rounded_full()
        .bg(theme::accent());
    let mut bar = div()
        .relative()
        .w_full()
        .h(height)
        .rounded(height)
        .bg(gpui::white().opacity(0.22))
        .child(
            div()
                .h_full()
                .w(relative(fraction))
                .rounded(height)
                .bg(theme::accent()),
        );
    if active {
        bar = bar.child(knob);
    } else if let Some(group) = group {
        bar = bar.child(knob.invisible().group_hover(group, |s| s.visible()));
    }
    bar
}

/// A text control on the video.
fn control(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    control_label(id, label)
        .cursor_pointer()
        .hover(|s| s.bg(gpui::white().opacity(0.12)))
}

/// `control` without hover or pointer, for one that is unavailable.
fn control_label(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .h(px(36.))
        .px(px(12.))
        .rounded(px(8.))
        .text_size(px(14.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::text())
        .whitespace_nowrap()
        .child(label.into())
}

/// The accent disc that plays and pauses.
fn play_button(paused: bool, cx: &mut Context<PlayerView>) -> impl IntoElement {
    let glyph: AnyElement = if paused {
        canvas(
            |_, _, _| {},
            |bounds, _, window, _| {
                let (o, s) = (bounds.origin, bounds.size);
                let mut path = PathBuilder::fill();
                path.move_to(o);
                path.line_to(point(o.x + s.width, o.y + s.height / 2.));
                path.line_to(point(o.x, o.y + s.height));
                path.close();
                if let Ok(path) = path.build() {
                    window.paint_path(path, theme::on_accent());
                }
            },
        )
        .w(px(15.))
        .h(px(17.))
        .ml(px(3.))
        .into_any_element()
    } else {
        let bar = || {
            div()
                .w(px(4.))
                .h(px(16.))
                .rounded(px(1.))
                .bg(theme::on_accent())
        };
        div()
            .flex()
            .gap(px(4.))
            .child(bar())
            .child(bar())
            .into_any_element()
    };
    div()
        .id("player-play")
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(48.))
        .rounded_full()
        .bg(theme::accent())
        .cursor_pointer()
        .hover(|s| s.bg(theme::accent_hover()))
        .on_click(cx.listener(|view, _, _, cx| view.toggle_pause(cx)))
        .child(glyph)
}

/// An accent sliver sweeping a thin track while something loads.
fn searching_bar() -> impl IntoElement {
    div()
        .relative()
        .w(px(220.))
        .h(px(3.))
        .rounded(px(2.))
        .overflow_hidden()
        .bg(gpui::white().opacity(0.1))
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
}
