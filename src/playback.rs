//! Playback flow rules shared by the player view and the Show page.

use std::path::Path;
use std::sync::{Mutex, PoisonError};

use anyhow::Result;

use crate::anicli::{self, ResolveOutcome};
use crate::db::Db;
use crate::model::{Mode, ShowSummary};
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
}
