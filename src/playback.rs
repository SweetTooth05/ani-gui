//! Playback flow rules shared by the player view and the Show page.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use anyhow::Result;

use crate::anicli::{self, ResolveOutcome};
use crate::db::Db;
use crate::model::{DlStatus, DownloadRecord, Mode, Progress, ShowSummary};
use crate::player::PlaybackState;
use crate::progress;

/// Seconds of playback movement between progress saves.
const SAVE_EVERY: f64 = 5.0;

/// The episode after `ep`, for "next episode" and autoplay.
pub fn next_episode(episodes: &[String], ep: &str) -> Option<String> {
    progress::next_after(ep, episodes)
}

/// Whether the position moved (forward or back) at least `SAVE_EVERY`
/// seconds since it was last saved.
pub fn should_save(last_saved_at: f64, now_pos: f64) -> bool {
    (now_pos - last_saved_at).abs() >= SAVE_EVERY
}

/// What the player should do after its state went from `prev` to `now`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Decision {
    pub save: bool,
    /// Start the countdown to the next episode.
    pub queue_next: bool,
    pub cancel_up_next: bool,
    /// The new value of "this episode's end was handled".
    pub eof_handled: bool,
}

/// Saves on every `SAVE_EVERY` seconds of movement, on pause and once at
/// the end, where it also queues the next episode if `can_queue` (autoplay
/// is on and there is one). Seeking back out of the end re-arms it.
/// Nothing happens before playback has `started`.
pub fn decide(
    prev: &PlaybackState,
    now: &PlaybackState,
    started: bool,
    saved_at: f64,
    eof_handled: bool,
    can_queue: bool,
) -> Decision {
    let mut d = Decision {
        eof_handled,
        ..Decision::default()
    };
    if !started {
        return d;
    }
    if now.eof && !eof_handled {
        d.save = true;
        d.queue_next = can_queue;
        d.eof_handled = true;
    } else {
        let paused = now.paused && !prev.paused && !now.eof;
        d.save = paused || should_save(saved_at, now.time_pos);
    }
    if prev.eof && !now.eof {
        d.cancel_up_next = true;
        d.eof_handled = false;
    }
    d
}

/// The position to save for `state`: the whole duration once it has ended.
pub fn saved_position(state: &PlaybackState) -> f64 {
    if state.eof && state.duration > 0.0 {
        state.duration
    } else {
        state.time_pos
    }
}

/// Where to start `ep` given the show's saved progress: its position unless
/// it was finished (or never started).
pub fn resume_point(saved: &[Progress], ep: &str) -> f64 {
    saved
        .iter()
        .find(|p| p.ep == ep && !progress::is_completed(p))
        .map_or(0.0, |p| p.position)
}

/// The finished download of `ep` (newest first in `downloads`), if any.
pub fn downloaded_file(
    downloads: &[DownloadRecord],
    show_id: &str,
    mode: Mode,
    ep: &str,
) -> Option<PathBuf> {
    downloads
        .iter()
        .filter(|d| d.show_id == show_id && d.mode == mode && d.ep == ep)
        .filter(|d| d.status == DlStatus::Done)
        .find_map(|d| d.path.as_ref().map(PathBuf::from))
}

/// Resolves `ep` of `show` with ani-cli (which records it in its history).
/// With `chosen`, that title (picked from ani-cli's results) becomes the
/// show's provider title first, so this and later runs search for it.
/// Blocks on ani-cli: call it off the UI thread.
pub fn resolve(
    db: &Mutex<Db>,
    shims: &Path,
    show: &ShowSummary,
    ep: &str,
    mode: Mode,
    quality: &str,
    chosen: Option<&str>,
) -> Result<ResolveOutcome> {
    let provider = {
        let db = db.lock().unwrap_or_else(PoisonError::into_inner);
        match chosen {
            Some(title) => {
                db.set_provider_title(&show.id, title)?;
                Some(title.to_string())
            }
            None => db.get_provider_title(&show.id)?,
        }
    };
    let req = anicli::build_request(show, provider.as_deref(), ep, mode, quality);
    anicli::resolve(&req, shims)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eps(list: &[&str]) -> Vec<String> {
        list.iter().map(|e| e.to_string()).collect()
    }

    #[test]
    fn next_episode_middle_and_last() {
        let list = eps(&["1", "2", "2.5", "3"]);
        assert_eq!(next_episode(&list, "1"), Some("2".into()));
        assert_eq!(next_episode(&list, "2"), Some("2.5".into()));
        assert_eq!(next_episode(&list, "3"), None);
        assert_eq!(next_episode(&[], "1"), None);
    }

    #[test]
    fn should_save_every_5s() {
        assert!(!should_save(0.0, 4.9));
        assert!(should_save(0.0, 5.0));
        assert!(!should_save(120.0, 123.0));
        assert!(should_save(120.0, 125.5));
        // A seek back counts as movement too.
        assert!(should_save(300.0, 20.0));
        assert!(!should_save(300.0, 298.0));
    }

    fn at(time_pos: f64) -> PlaybackState {
        PlaybackState {
            time_pos,
            duration: 1440.0,
            ..PlaybackState::default()
        }
    }

    #[test]
    fn decide_saves_every_5s_of_movement() {
        let d = decide(&at(14.0), &at(14.9), true, 10.0, false, true);
        assert_eq!(d, Decision::default());
        let d = decide(&at(14.9), &at(15.0), true, 10.0, false, true);
        assert!(d.save && !d.queue_next && !d.cancel_up_next && !d.eof_handled);
        // Nothing before playback has started.
        let d = decide(&at(0.0), &at(30.0), false, 0.0, false, true);
        assert_eq!(d, Decision::default());
    }

    #[test]
    fn decide_saves_on_pause() {
        let paused = PlaybackState {
            paused: true,
            ..at(12.0)
        };
        assert!(decide(&at(12.0), &paused, true, 10.0, false, true).save);
        assert!(!decide(&paused, &paused, true, 10.0, false, true).save);
    }

    #[test]
    fn decide_eof_saves_and_queues_once() {
        let end = PlaybackState {
            eof: true,
            ..at(1440.0)
        };
        let d = decide(&at(1439.0), &end, true, 1436.0, false, true);
        assert_eq!(
            d,
            Decision {
                save: true,
                queue_next: true,
                cancel_up_next: false,
                eof_handled: true,
            }
        );
        assert_eq!(
            saved_position(&PlaybackState {
                time_pos: 1439.6,
                ..end
            }),
            1440.0
        );
        // mpv then pauses at the end: no second save or queue.
        let paused_end = PlaybackState {
            paused: true,
            ..end
        };
        let d = decide(&end, &paused_end, true, 1440.0, true, true);
        assert_eq!(
            d,
            Decision {
                eof_handled: true,
                ..Decision::default()
            }
        );
    }

    #[test]
    fn decide_seeking_out_of_eof_cancels_up_next() {
        let end = PlaybackState {
            eof: true,
            paused: true,
            ..at(1440.0)
        };
        let back = PlaybackState {
            paused: true,
            ..at(1300.0)
        };
        let d = decide(&end, &back, true, 1440.0, true, true);
        assert!(d.cancel_up_next && !d.eof_handled && !d.queue_next);
        assert!(d.save, "the seek back moved more than 5 s");
        assert_eq!(saved_position(&back), 1300.0);
    }

    #[test]
    fn decide_last_episode_does_not_queue() {
        let end = PlaybackState {
            eof: true,
            ..at(1440.0)
        };
        let d = decide(&at(1439.0), &end, true, 1436.0, false, false);
        assert!(d.save && d.eof_handled && !d.queue_next);
    }

    fn progress(ep: &str, position: f64) -> Progress {
        Progress {
            show_id: "s".into(),
            mode: Mode::Sub,
            ep: ep.into(),
            position,
            duration: 1400.0,
            updated_at: 0,
        }
    }

    #[test]
    fn resume_point_partial_only() {
        let saved = [progress("1", 1390.0), progress("2", 312.5)];
        assert_eq!(resume_point(&saved, "2"), 312.5);
        assert_eq!(resume_point(&saved, "1"), 0.0, "finished episodes restart");
        assert_eq!(resume_point(&saved, "3"), 0.0);
    }

    fn download(
        id: &str,
        ep: &str,
        mode: Mode,
        status: DlStatus,
        path: Option<&str>,
    ) -> DownloadRecord {
        DownloadRecord {
            id: id.into(),
            show_id: "s".into(),
            title: "Show".into(),
            ep: ep.into(),
            mode,
            quality: "best".into(),
            path: path.map(str::to_string),
            status,
            progress: 100.0,
            error: None,
            created_at: 0,
        }
    }

    #[test]
    fn downloaded_file_needs_done_with_path() {
        let list = [
            download("a", "2", Mode::Sub, DlStatus::Failed, None),
            download("b", "2", Mode::Sub, DlStatus::Done, Some("/v/s-2.mp4")),
            download("c", "3", Mode::Dub, DlStatus::Done, Some("/v/s-3-dub.mp4")),
            download("d", "4", Mode::Sub, DlStatus::Done, None),
            download("e", "5", Mode::Sub, DlStatus::Running, Some("/v/s-5.mp4")),
        ];
        assert_eq!(
            downloaded_file(&list, "s", Mode::Sub, "2"),
            Some(PathBuf::from("/v/s-2.mp4"))
        );
        assert_eq!(downloaded_file(&list, "s", Mode::Sub, "3"), None);
        assert_eq!(downloaded_file(&list, "s", Mode::Sub, "4"), None);
        assert_eq!(downloaded_file(&list, "s", Mode::Sub, "5"), None);
        assert_eq!(downloaded_file(&list, "other", Mode::Dub, "3"), None);
    }
}
