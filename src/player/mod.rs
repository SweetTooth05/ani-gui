//! libmpv playback with software rendering into BGRA frames.
//!
//! A render thread turns mpv's update callbacks into `Frame`s at the
//! requested size; an event thread folds observed properties into
//! `PlaybackState`. Both call `on_change` so a view can repaint.

mod ffi;

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::{iter, ptr};

use crate::model::Stream;
use ffi::{Api, MpvEventEndFile, MpvEventProperty, MpvHandle, MpvRenderContext, MpvRenderParam};

/// libmpv's file names, tried in order. On macOS the bare names go through
/// dyld's search path, which an app started from Finder doesn't extend with
/// Homebrew's lib folders, so those are tried by full path too.
const LIBRARIES: &[&str] = if cfg!(target_os = "macos") {
    &[
        "libmpv.2.dylib",
        "/opt/homebrew/lib/libmpv.2.dylib",
        "/usr/local/lib/libmpv.2.dylib",
        "libmpv.dylib",
        "/opt/homebrew/lib/libmpv.dylib",
        "/usr/local/lib/libmpv.dylib",
    ]
} else {
    &["libmpv.so.2"]
};
const HWDEC: &CStr = c"auto-copy";
const DEFAULT_SIZE: (u32, u32) = (1280, 720);
const MAX_SIDE: u32 = 16384;
const STRIDE_ALIGN: usize = 64;

/// Observed properties; the index is the `reply_userdata` mpv echoes back.
const PROPERTIES: [(&CStr, c_int); 7] = [
    (c"time-pos", ffi::MPV_FORMAT_DOUBLE),
    (c"duration", ffi::MPV_FORMAT_DOUBLE),
    (c"pause", ffi::MPV_FORMAT_FLAG),
    (c"eof-reached", ffi::MPV_FORMAT_FLAG),
    (c"paused-for-cache", ffi::MPV_FORMAT_FLAG),
    (c"volume", ffi::MPV_FORMAT_DOUBLE),
    (c"sub-visibility", ffi::MPV_FORMAT_FLAG),
];

pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// BGRA, stride = width * 4, alpha always 255.
    pub data: Vec<u8>,
    pub serial: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlaybackState {
    pub time_pos: f64,
    pub duration: f64,
    pub paused: bool,
    pub eof: bool,
    pub buffering: bool,
    pub volume: f64,
    pub sub_visible: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlayerError {
    LibraryMissing(String),
    Mpv(String),
}

impl fmt::Display for PlayerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlayerError::LibraryMissing(msg) => write!(f, "libmpv could not be loaded: {msg}"),
            PlayerError::Mpv(msg) => write!(f, "mpv: {msg}"),
        }
    }
}

impl std::error::Error for PlayerError {}

type OnChange = Arc<dyn Fn() + Send + Sync>;

struct Handle(*mut MpvHandle);
// SAFETY: the mpv client API is thread-safe; the handle stays valid until
// `MpvPlayer::drop` has joined every thread that uses it.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

struct RenderCtx(*mut MpvRenderContext);
// SAFETY: only the render thread touches the context after creation.
unsafe impl Send for RenderCtx {}

#[derive(Default)]
struct Wake {
    update: bool,
    redraw: bool,
    shutdown: bool,
}

#[derive(Default)]
struct FrameSlot {
    latest: Option<Arc<Frame>>,
    /// Bumped by `load`, `stop` and load failures so a render begun before
    /// them can't publish its frame afterwards.
    generation: u64,
    /// False from `stop` until the next `load`: mpv may still hand out
    /// frames while it tears playback down.
    live: bool,
    /// A frame was published since the last clear, even if it was taken.
    shown: bool,
}

impl FrameSlot {
    fn clear(&mut self) {
        self.generation += 1;
        self.latest = None;
        self.shown = false;
    }

    fn publish(&mut self, frame: Frame) {
        self.latest = Some(Arc::new(frame));
        self.shown = true;
    }

    fn take(&mut self) -> Option<Frame> {
        let frame = self.latest.take()?;
        Some(Arc::try_unwrap(frame).unwrap_or_else(|f| Frame {
            width: f.width,
            height: f.height,
            data: f.data.clone(),
            serial: f.serial,
        }))
    }
}

struct Shared {
    api: Api,
    mpv: Handle,
    state: Mutex<PlaybackState>,
    frame: Mutex<FrameSlot>,
    last_error: Mutex<Option<String>>,
    render_size: Mutex<(u32, u32)>,
    wake: Mutex<Wake>,
    wake_cv: Condvar,
    events_shutdown: AtomicBool,
    on_change: Mutex<Option<OnChange>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn signal(&self, set: impl FnOnce(&mut Wake)) {
        set(&mut lock(&self.wake));
        self.wake_cv.notify_one();
    }

    fn notify(&self) {
        let cb = lock(&self.on_change).clone();
        if let Some(cb) = cb {
            cb();
        }
    }

    fn check(&self, rc: c_int) -> Result<(), PlayerError> {
        if rc < 0 {
            Err(PlayerError::Mpv(self.api.error_message(rc)))
        } else {
            Ok(())
        }
    }

    fn command(&self, args: &[&str]) -> Result<(), PlayerError> {
        let rc = self.command_rc(args)?;
        self.check(rc)
    }

    /// Runs a command and returns mpv's raw status code.
    fn command_rc(&self, args: &[&str]) -> Result<c_int, PlayerError> {
        let owned = args
            .iter()
            .map(|a| CString::new(*a))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| PlayerError::Mpv("command argument contains a NUL byte".into()))?;
        let mut argv: Vec<*const c_char> = owned
            .iter()
            .map(|a| a.as_ptr())
            .chain(iter::once(ptr::null()))
            .collect();
        // SAFETY: argv is a NULL-terminated array of C strings outliving the call.
        Ok(unsafe { (self.api.command)(self.mpv.0, argv.as_mut_ptr()) })
    }

    fn set_property(&self, name: &str, value: &str) -> Result<(), PlayerError> {
        let (Ok(name), Ok(value)) = (CString::new(name), CString::new(value)) else {
            return Err(PlayerError::Mpv("property contains a NUL byte".into()));
        };
        // SAFETY: valid handle and C strings.
        let rc =
            unsafe { (self.api.set_property_string)(self.mpv.0, name.as_ptr(), value.as_ptr()) };
        self.check(rc)
    }
}

/// mpv render update callback: runs on mpv's threads, so it only wakes the
/// render thread and never calls back into mpv.
unsafe extern "C" fn on_render_update(ctx: *mut c_void) {
    // SAFETY: ctx is `Arc::as_ptr` of a `Shared` the render thread keeps alive
    // until after `mpv_render_context_free`.
    let shared = unsafe { &*(ctx as *const Shared) };
    shared.signal(|w| w.update = true);
}

/// Whether libmpv loads with every symbol the player uses, without starting
/// mpv. The library stays loaded: unloading it buys nothing and libmpv is
/// not written to be unloaded.
pub fn probe_library() -> Result<(), String> {
    let api = load_api(LIBRARIES)?;
    std::mem::forget(api);
    Ok(())
}

/// The first of `names` that loads with every symbol. The error names the
/// first failure (the most likely location) and counts the rest: dyld's
/// messages list every path it tried, so all of them would be unreadable.
fn load_api(names: &[&str]) -> Result<Api, String> {
    let mut first = None;
    for name in names {
        match Api::load(name) {
            Ok(api) => return Ok(api),
            Err(e) => {
                first.get_or_insert_with(|| format!("{name}: {e}"));
            }
        }
    }
    let first = first.unwrap_or_else(|| "no library name to load".into());
    Err(match names.len() {
        0 | 1 => first,
        n => format!("{first} (and {} other locations)", n - 1),
    })
}

pub struct MpvPlayer {
    shared: Arc<Shared>,
    render_thread: Option<JoinHandle<()>>,
    event_thread: Option<JoinHandle<()>>,
}

impl MpvPlayer {
    pub fn new() -> Result<MpvPlayer, PlayerError> {
        MpvPlayer::with_library(LIBRARIES, HWDEC)
    }

    fn with_library(names: &[&str], hwdec: &CStr) -> Result<MpvPlayer, PlayerError> {
        let api = load_api(names).map_err(PlayerError::LibraryMissing)?;
        // SAFETY: plain constructor.
        let mpv = unsafe { (api.create)() };
        if mpv.is_null() {
            return Err(PlayerError::Mpv("mpv_create failed".into()));
        }
        let render_ctx = match init(&api, mpv, hwdec) {
            Ok(ctx) => ctx,
            Err(err) => {
                // SAFETY: mpv is a valid handle with no render context.
                unsafe { (api.terminate_destroy)(mpv) };
                return Err(err);
            }
        };

        let shared = Arc::new(Shared {
            api,
            mpv: Handle(mpv),
            state: Mutex::new(PlaybackState::default()),
            frame: Mutex::new(FrameSlot::default()),
            last_error: Mutex::new(None),
            render_size: Mutex::new(DEFAULT_SIZE),
            wake: Mutex::new(Wake::default()),
            wake_cv: Condvar::new(),
            events_shutdown: AtomicBool::new(false),
            on_change: Mutex::new(None),
        });
        // SAFETY: the callback context outlives the render context (see
        // `on_render_update`).
        unsafe {
            (shared.api.render_context_set_update_callback)(
                render_ctx.0,
                Some(on_render_update),
                Arc::as_ptr(&shared) as *mut c_void,
            );
        }

        let ctx_ptr = render_ctx.0;
        let render_thread = {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("mpv-render".into())
                .spawn(move || render_loop(&shared, render_ctx))
        };
        let render_thread = match render_thread {
            Ok(t) => t,
            Err(err) => {
                // SAFETY: the thread never ran, so nothing else uses ctx or mpv.
                unsafe {
                    (shared.api.render_context_free)(ctx_ptr);
                    (shared.api.terminate_destroy)(shared.mpv.0);
                }
                return Err(PlayerError::Mpv(format!("spawning render thread: {err}")));
            }
        };
        let event_thread = {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("mpv-events".into())
                .spawn(move || event_loop(&shared))
        };
        let mut player = MpvPlayer {
            shared,
            render_thread: Some(render_thread),
            event_thread: None,
        };
        match event_thread {
            Ok(t) => player.event_thread = Some(t),
            Err(err) => return Err(PlayerError::Mpv(format!("spawning event thread: {err}"))),
        }
        Ok(player)
    }

    /// Starts `s` (a URL, or a local path in `s.url`) at `start_secs`.
    /// Failures to open it arrive later through `last_error`.
    pub fn load(&self, s: &Stream, start_secs: f64) -> Result<(), PlayerError> {
        *lock(&self.shared.last_error) = None;
        {
            let mut slot = lock(&self.shared.frame);
            slot.generation += 1;
            slot.live = true;
        }
        let opts = loadfile_options(s, start_secs);
        let mut rc = self
            .shared
            .command_rc(&loadfile_args(&s.url, &opts, true))?;
        if rc == ffi::MPV_ERROR_INVALID_PARAMETER {
            // mpv < 0.38 has no index argument.
            rc = self
                .shared
                .command_rc(&loadfile_args(&s.url, &opts, false))?;
        }
        self.shared.check(rc)?;
        // `pause` outlives the file, and keep-open sets it at the end. Unpaused
        // only now: before `loadfile` replaced it, an ended file would pause
        // again at once and the new one would inherit that.
        self.set_pause(false);
        Ok(())
    }

    /// Why the last loaded stream failed to play, until the next `load`.
    pub fn last_error(&self) -> Option<String> {
        lock(&self.shared.last_error).clone()
    }

    // Controls are best-effort; mpv rejects most of them while nothing is loaded.

    pub fn toggle_pause(&self) {
        let _ = self.shared.command(&["cycle", "pause"]);
    }

    pub fn set_pause(&self, paused: bool) {
        let _ = self
            .shared
            .set_property("pause", if paused { "yes" } else { "no" });
    }

    pub fn seek_relative(&self, secs: f64) {
        if secs.is_finite() {
            let _ = self
                .shared
                .command(&["seek", &secs.to_string(), "relative"]);
        }
    }

    pub fn seek_absolute(&self, secs: f64) {
        if secs.is_finite() {
            let _ = self
                .shared
                .command(&["seek", &secs.to_string(), "absolute"]);
        }
    }

    pub fn set_volume(&self, volume: f64) {
        if volume.is_finite() {
            let _ = self.shared.set_property("volume", &volume.to_string());
        }
    }

    pub fn toggle_subs(&self) {
        let _ = self.shared.command(&["cycle", "sub-visibility"]);
    }

    pub fn stop(&self) {
        let _ = self.shared.command(&["stop"]);
        let mut slot = lock(&self.shared.frame);
        slot.clear();
        slot.live = false;
    }

    /// Size of the frames rendered from now on (clamped to at least 1×1).
    pub fn set_render_size(&self, w: u32, h: u32) {
        let size = (w.clamp(1, MAX_SIDE), h.clamp(1, MAX_SIDE));
        let changed = {
            let mut current = lock(&self.shared.render_size);
            let changed = *current != size;
            *current = size;
            changed
        };
        if changed {
            self.shared.signal(|w| w.redraw = true);
        }
    }

    /// Moves the newest frame out; `None` until the next one is rendered.
    pub fn take_frame(&self) -> Option<Frame> {
        lock(&self.shared.frame).take()
    }

    pub fn state(&self) -> PlaybackState {
        *lock(&self.shared.state)
    }

    /// Called from the player's threads after every new frame or state change.
    pub fn set_on_change(&self, f: Box<dyn Fn() + Send + Sync>) {
        *lock(&self.shared.on_change) = Some(Arc::from(f));
    }
}

impl Drop for MpvPlayer {
    fn drop(&mut self) {
        self.shared.signal(|w| w.shutdown = true);
        if let Some(t) = self.render_thread.take() {
            let _ = t.join();
        }
        self.shared.events_shutdown.store(true, Ordering::SeqCst);
        // SAFETY: valid handle; mpv_wakeup is callable from any thread.
        unsafe { (self.shared.api.wakeup)(self.shared.mpv.0) };
        if let Some(t) = self.event_thread.take() {
            let _ = t.join();
        }
        // SAFETY: the render context is freed and no thread uses the handle.
        unsafe { (self.shared.api.terminate_destroy)(self.shared.mpv.0) };
    }
}

/// Sets options, initializes mpv, observes properties and creates the SW
/// render context (which must exist before a file loads with `vo=libmpv`).
fn init(api: &Api, mpv: *mut MpvHandle, hwdec: &CStr) -> Result<RenderCtx, PlayerError> {
    let check = |rc: c_int| {
        if rc < 0 {
            Err(PlayerError::Mpv(api.error_message(rc)))
        } else {
            Ok(())
        }
    };
    for (name, value) in [
        (c"vo", c"libmpv"),
        (c"hwdec", hwdec),
        (c"keep-open", c"yes"),
    ] {
        // SAFETY: valid handle and static C strings.
        check(unsafe { (api.set_option_string)(mpv, name.as_ptr(), value.as_ptr()) })?;
    }
    // SAFETY: valid, not yet initialized handle.
    check(unsafe { (api.initialize)(mpv) })?;
    for (id, (name, format)) in PROPERTIES.iter().enumerate() {
        // SAFETY: valid handle and static C string.
        check(unsafe { (api.observe_property)(mpv, id as u64, name.as_ptr(), *format) })?;
    }
    let mut params = [
        MpvRenderParam {
            kind: ffi::MPV_RENDER_PARAM_API_TYPE,
            data: ffi::MPV_RENDER_API_TYPE_SW.as_ptr() as *mut c_void,
        },
        MpvRenderParam {
            kind: ffi::MPV_RENDER_PARAM_INVALID,
            data: ptr::null_mut(),
        },
    ];
    let mut ctx = ptr::null_mut();
    // SAFETY: params is terminated by PARAM_INVALID; ctx is an out-pointer.
    check(unsafe { (api.render_context_create)(&mut ctx, mpv, params.as_mut_ptr()) })?;
    Ok(RenderCtx(ctx))
}

/// `loadfile` per-file options, each value `%len%`-quoted so URLs and paths
/// may contain commas or `=`.
fn loadfile_options(s: &Stream, start_secs: f64) -> String {
    let start = (start_secs.is_finite() && start_secs > 0.0).then(|| start_secs.to_string());
    [
        ("referrer", s.referrer.as_deref()),
        ("audio-file", s.audio_file.as_deref()),
        ("sub-file", s.sub_file.as_deref()),
        ("force-media-title", s.title.as_deref()),
        ("start", start.as_deref()),
    ]
    .into_iter()
    .filter_map(|(key, value)| value.map(|v| format!("{key}=%{}%{v}", v.len())))
    .collect::<Vec<_>>()
    .join(",")
}

/// `loadfile` argv: mpv ≥ 0.38 takes an insert index before the options,
/// older versions take the options third.
fn loadfile_args<'a>(url: &'a str, opts: &'a str, with_index: bool) -> Vec<&'a str> {
    let mut args = vec!["loadfile", url, "replace"];
    if with_index {
        args.push("-1");
    }
    args.push(opts);
    args
}

fn render_loop(shared: &Shared, ctx: RenderCtx) {
    let api = &shared.api;
    let mut buf = Vec::new();
    let mut serial = 0;
    loop {
        let (update, redraw) = {
            let mut wake = lock(&shared.wake);
            while !(wake.update || wake.redraw || wake.shutdown) {
                wake = shared
                    .wake_cv
                    .wait(wake)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            if wake.shutdown {
                break;
            }
            let pending = (wake.update, wake.redraw);
            wake.update = false;
            wake.redraw = false;
            pending
        };
        // SAFETY: called on the render thread, never from the callback.
        let flags = if update {
            unsafe { (api.render_context_update)(ctx.0) }
        } else {
            0
        };
        // A redraw (after a resize) only makes sense once video is showing.
        let redraw = redraw && lock(&shared.frame).shown;
        if flags & ffi::MPV_RENDER_UPDATE_FRAME == 0 && !redraw {
            continue;
        }
        let (w, h) = *lock(&shared.render_size);
        let generation = lock(&shared.frame).generation;
        let Some(data) = render(api, ctx.0, w, h, &mut buf) else {
            continue;
        };
        {
            let mut slot = lock(&shared.frame);
            if !slot.live || slot.generation != generation {
                continue;
            }
            serial += 1;
            slot.publish(Frame {
                width: w,
                height: h,
                data,
                serial,
            });
        }
        shared.notify();
    }
    // SAFETY: render thread owns ctx; freed before mpv_terminate_destroy.
    unsafe {
        (api.render_context_set_update_callback)(ctx.0, None, ptr::null_mut());
        (api.render_context_free)(ctx.0);
    }
}

/// Renders one `w`×`h` frame into a 64-byte aligned scratch buffer and
/// returns it repacked as tight BGRA.
fn render(
    api: &Api,
    ctx: *mut MpvRenderContext,
    w: u32,
    h: u32,
    buf: &mut Vec<u8>,
) -> Option<Vec<u8>> {
    let row = w as usize * 4;
    let mut stride = row.next_multiple_of(STRIDE_ALIGN);
    let len = stride * h as usize;
    buf.resize(len + STRIDE_ALIGN, 0);
    let offset = buf.as_ptr().align_offset(STRIDE_ALIGN);
    let target = &mut buf[offset..offset + len];
    let mut size = [w as c_int, h as c_int];
    let mut params = [
        MpvRenderParam {
            kind: ffi::MPV_RENDER_PARAM_SW_SIZE,
            data: size.as_mut_ptr() as *mut c_void,
        },
        MpvRenderParam {
            kind: ffi::MPV_RENDER_PARAM_SW_FORMAT,
            data: c"bgr0".as_ptr() as *mut c_void,
        },
        MpvRenderParam {
            kind: ffi::MPV_RENDER_PARAM_SW_STRIDE,
            data: &mut stride as *mut usize as *mut c_void,
        },
        MpvRenderParam {
            kind: ffi::MPV_RENDER_PARAM_SW_POINTER,
            data: target.as_mut_ptr() as *mut c_void,
        },
        MpvRenderParam {
            kind: ffi::MPV_RENDER_PARAM_INVALID,
            data: ptr::null_mut(),
        },
    ];
    // SAFETY: target holds stride*h writable bytes at a 64-byte aligned
    // address; params is terminated by PARAM_INVALID.
    let rc = unsafe { (api.render_context_render)(ctx, params.as_mut_ptr()) };
    if rc < 0 {
        return None;
    }
    let mut out = Vec::with_capacity(row * h as usize);
    for line in target.chunks_exact(stride) {
        out.extend_from_slice(&line[..row]);
    }
    for px in out.as_chunks_mut::<4>().0 {
        px[3] = 0xff;
    }
    Some(out)
}

fn event_loop(shared: &Shared) {
    let api = &shared.api;
    loop {
        if shared.events_shutdown.load(Ordering::SeqCst) {
            break;
        }
        // SAFETY: valid handle; only this thread waits on events.
        let event = unsafe { (api.wait_event)(shared.mpv.0, -1.0) };
        if event.is_null() {
            continue;
        }
        // SAFETY: mpv keeps the event valid until the next mpv_wait_event.
        let event = unsafe { &*event };
        match event.event_id {
            ffi::MPV_EVENT_SHUTDOWN => break,
            // Events are ordered, so this also drops an error from a file
            // replaced before its failure was reported.
            ffi::MPV_EVENT_START_FILE => *lock(&shared.last_error) = None,
            ffi::MPV_EVENT_END_FILE if !event.data.is_null() => {
                // SAFETY: END_FILE carries an mpv_event_end_file.
                let end = unsafe { &*(event.data as *const MpvEventEndFile) };
                if end.reason == ffi::MPV_END_FILE_REASON_ERROR {
                    *lock(&shared.last_error) = Some(api.error_message(end.error));
                    lock(&shared.frame).clear();
                    shared.notify();
                }
            }
            ffi::MPV_EVENT_PROPERTY_CHANGE if !event.data.is_null() => {
                // SAFETY: PROPERTY_CHANGE carries an mpv_event_property.
                let prop = unsafe { &*(event.data as *const MpvEventProperty) };
                apply_property(&mut lock(&shared.state), event.reply_userdata, prop);
                shared.notify();
            }
            _ => {}
        }
    }
}

fn apply_property(state: &mut PlaybackState, id: u64, prop: &MpvEventProperty) {
    let has_data = !prop.data.is_null();
    // SAFETY: data points to a double / int flag when format says so.
    let double = if has_data && prop.format == ffi::MPV_FORMAT_DOUBLE {
        unsafe { *(prop.data as *const f64) }
    } else {
        0.0
    };
    let flag = has_data
        && prop.format == ffi::MPV_FORMAT_FLAG
        && unsafe { *(prop.data as *const c_int) } != 0;
    match id {
        0 => state.time_pos = double,
        1 => state.duration = double,
        2 => state.paused = flag,
        3 => state.eof = flag,
        4 => state.buffering = flag,
        5 => state.volume = double,
        6 => state.sub_visible = flag,
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn frame(serial: u64) -> Frame {
        Frame {
            width: 2,
            height: 1,
            data: vec![1, 2, 3, 255, 4, 5, 6, 255],
            serial,
        }
    }

    #[test]
    fn frame_slot_take_moves_frame_out() {
        let mut slot = FrameSlot::default();
        assert!(slot.take().is_none());
        slot.publish(frame(1));
        let data = slot.latest.as_ref().expect("published").data.as_ptr();
        let taken = slot.take().expect("frame");
        assert_eq!(taken.serial, 1);
        assert_eq!(taken.data.as_ptr(), data, "moved, not copied");
        assert!(slot.take().is_none(), "taken frames don't come back");
        assert!(slot.shown, "video is still showing after a take");
        slot.publish(frame(2));
        slot.clear();
        assert!(slot.take().is_none());
        assert!(!slot.shown);
    }

    #[test]
    fn loadfile_options_quote_values() {
        let s = Stream {
            url: "https://x/a.m3u8".into(),
            referrer: Some("https://r.example/".into()),
            audio_file: None,
            sub_file: Some("https://s/a,b.vtt".into()),
            title: Some("Ep 1".into()),
        };
        assert_eq!(
            loadfile_options(&s, 12.5),
            "referrer=%18%https://r.example/,sub-file=%17%https://s/a,b.vtt,\
             force-media-title=%4%Ep 1,start=%4%12.5"
        );
        assert_eq!(loadfile_options(&s, 0.0).matches("start=").count(), 0);
    }

    #[test]
    fn loadfile_args_both_forms() {
        assert_eq!(
            loadfile_args("u", "o=%1%x", true),
            ["loadfile", "u", "replace", "-1", "o=%1%x"]
        );
        assert_eq!(
            loadfile_args("u", "o=%1%x", false),
            ["loadfile", "u", "replace", "o=%1%x"]
        );
    }

    /// Hardware decoding off keeps tests quiet and independent of the GPU.
    fn test_player() -> Option<MpvPlayer> {
        match MpvPlayer::with_library(LIBRARIES, c"no") {
            Ok(p) => Some(p),
            Err(PlayerError::LibraryMissing(msg)) => {
                eprintln!("skipping: libmpv not available: {msg}");
                None
            }
            Err(err) => panic!("player init failed: {err}"),
        }
    }

    #[test]
    fn load_missing_file_reports_error() {
        let Some(player) = test_player() else { return };
        let stream = Stream {
            url: "/nonexistent/ani-gui-player-test.mp4".into(),
            referrer: None,
            audio_file: None,
            sub_file: None,
            title: None,
        };
        player.load(&stream, 0.0).expect("load");
        assert!(
            wait_for(Duration::from_secs(2), || player.last_error().is_some()),
            "no error reported for a missing file"
        );
    }

    #[test]
    fn load_library_missing_returns_error() {
        match MpvPlayer::with_library(&["libdoes-not-exist.so"], c"no") {
            Err(PlayerError::LibraryMissing(msg)) => {
                assert!(msg.starts_with("libdoes-not-exist.so: "), "{msg}");
            }
            Err(other) => panic!("expected LibraryMissing, got {other}"),
            Ok(_) => panic!("expected LibraryMissing, got a player"),
        }
    }

    #[test]
    fn every_library_name_is_tried_and_the_first_failure_reported() {
        let Err(msg) = load_api(&["libnope-a.so", "/nonexistent/libnope-b.so", "libnope-c.so"])
        else {
            panic!("nothing should load");
        };
        assert!(msg.starts_with("libnope-a.so: "), "{msg}");
        assert!(msg.ends_with(" (and 2 other locations)"), "{msg}");
        assert!(load_api(&[]).is_err());
        // The real candidates for this platform.
        if cfg!(target_os = "macos") {
            assert_eq!(LIBRARIES[0], "libmpv.2.dylib");
        } else {
            assert_eq!(LIBRARIES, ["libmpv.so.2"]);
        }
    }

    fn make_clip(dir: &Path) -> Option<PathBuf> {
        let clip = dir.join("t.mp4");
        let status = Command::new("ffmpeg")
            .args(["-loglevel", "error", "-f", "lavfi", "-i"])
            .arg("testsrc=duration=3:size=320x240:rate=25")
            .arg("-y")
            .arg(&clip)
            .stdin(Stdio::null())
            .status()
            .ok()?;
        status.success().then_some(clip)
    }

    fn wait_for(timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if check() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        check()
    }

    #[test]
    fn plays_local_file_and_produces_frames() {
        let dir = std::env::temp_dir().join(format!("ani-gui-player-test-{}", std::process::id()));
        if let Err(err) = std::fs::create_dir_all(&dir) {
            eprintln!("skipping: cannot create {}: {err}", dir.display());
            return;
        }
        let Some(clip) = make_clip(&dir) else {
            eprintln!("skipping: ffmpeg is not available to generate a test clip");
            return;
        };
        let Some(player) = test_player() else { return };
        let stream = Stream {
            url: clip.to_string_lossy().into_owned(),
            referrer: None,
            audio_file: None,
            sub_file: None,
            title: None,
        };
        player.load(&stream, 0.0).expect("load");

        let mut frame = None;
        assert!(
            wait_for(Duration::from_secs(5), || {
                frame = player.take_frame();
                frame.as_ref().is_some_and(|f| f.width > 0)
            }),
            "no frame within 5 s"
        );
        let frame = frame.expect("frame");
        assert_eq!(frame.data.len(), (frame.width * frame.height * 4) as usize);
        let pixels = frame.data.as_chunks::<4>().0;
        assert!(pixels.iter().all(|px| px[3] == 0xff));
        assert!(
            pixels.iter().any(|px| px[..3] != [0, 0, 0]),
            "frame is all black"
        );
        assert_eq!(player.last_error(), None);

        assert!(
            wait_for(Duration::from_secs(5), || (player.state().duration - 3.0)
                .abs()
                <= 0.2),
            "duration was {}",
            player.state().duration
        );
        let first = player.state().time_pos;
        assert!(
            wait_for(Duration::from_secs(5), || player.state().time_pos > first),
            "time_pos did not advance from {first}"
        );

        player.seek_absolute(2.0);
        assert!(
            wait_for(Duration::from_secs(2), || player.state().time_pos >= 1.9),
            "time_pos after seek was {}",
            player.state().time_pos
        );

        // Every frame so far was taken: a paused or ended clip must still be
        // redrawn at the new size.
        player.set_render_size(160, 120);
        assert!(
            wait_for(Duration::from_secs(5), || player
                .take_frame()
                .is_some_and(|f| f.width == 160 && f.height == 120)),
            "no 160x120 frame after resize"
        );

        // mpv pauses at the end (keep-open); the next load still plays.
        assert!(
            wait_for(Duration::from_secs(5), || {
                let s = player.state();
                s.eof && s.paused
            }),
            "clip did not end paused: {:?}",
            player.state()
        );
        player.load(&stream, 0.0).expect("reload");
        assert!(
            wait_for(Duration::from_secs(5), || {
                let s = player.state();
                !s.paused && !s.eof && s.time_pos > 0.2
            }),
            "next load did not play: {:?}",
            player.state()
        );

        player.stop();
        std::thread::sleep(Duration::from_millis(300));
        assert!(player.take_frame().is_none(), "frame came back after stop");

        drop(player);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
