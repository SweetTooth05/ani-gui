//! Opening/ending skip timestamps (AniSkip API with mpv chapter fallback) and
//! the Netflix-style prompt visibility state machine for "Skip intro" and
//! "Next episode".

use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::catalog;
use crate::db::Db;

const ANISKIP_API: &str = "https://api.aniskip.com/v2/skip-times";
const CACHE_KEY_PREFIX: &str = "aniskip:v1";
/// Skip times rarely change once submitted; cache for a week.
const CACHE_TTL_SECS: i64 = 7 * 24 * 60 * 60;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
/// A real answer is a few hundred bytes; anything near this is not AniSkip.
const MAX_RESPONSE_BYTES: u64 = 64 * 1024;
const USER_AGENT: &str = "ani-gui/1.0";

/// How long a floating Skip Intro / Next Episode button stays up while the
/// rest of the HUD is hidden before fading out.
pub const PROMPT_AUTO_HIDE: Duration = Duration::from_secs(8);

/// When neither AniSkip nor mpv chapters provide an ending timestamp, show
/// "Next episode" in the last 90 s of full-length episodes.
const FALLBACK_OUTRO_SECS: f64 = 90.0;
/// Shorter episodes (e.g. 2-minute shorts) use the last 15% instead of 90 s.
const FALLBACK_MIN_FULL_DURATION: f64 = 180.0;
const FALLBACK_MIN_SHORT_DURATION: f64 = 30.0;
const FALLBACK_SHORT_RATIO: f64 = 0.85;
/// Maximum difference between an AniSkip entry's recorded episode length and
/// the stream's actual duration to treat it as an exact length match.
const LENGTH_MATCH_TOLERANCE_SECS: f64 = 30.0;
/// Minimum duration of a valid intro/outro segment.
const MIN_SEGMENT_SECS: f64 = 1.0;

static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .user_agent(USER_AGENT)
        .http_status_as_error(false)
        .build()
        .into()
});

/// One opening or ending interval in seconds from the start of the episode.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
}

impl Segment {
    pub fn is_valid(&self) -> bool {
        self.start.is_finite()
            && self.end.is_finite()
            && self.start >= 0.0
            && self.end - self.start >= MIN_SEGMENT_SECS
    }

    pub fn contains(&self, pos: f64) -> bool {
        pos.is_finite() && pos >= self.start && pos < self.end
    }
}

/// One chapter reported by mpv's `chapter-list` property.
#[derive(Clone, Debug, PartialEq)]
pub struct Chapter {
    pub title: String,
    pub time: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkipKind {
    Op,
    MixedOp,
    Ed,
    MixedEd,
}

/// One parsed entry from AniSkip's `results` array, cached in `api_cache`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AniSkipEntry {
    pub kind: SkipKind,
    pub interval: Segment,
    pub episode_length: f64,
}

/// Resolved opening and ending segments for the currently playing episode.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SkipTimes {
    pub intro: Option<Segment>,
    pub outro: Option<Segment>,
}

/// Which contextual skip/advance prompt is eligible at the current timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivePrompt {
    SkipIntro,
    NextEpisode,
}

/// Tracks entry into an intro/outro segment so its floating button stays
/// visible for [`PROMPT_AUTO_HIDE`] even while the HUD is hidden, then hides
/// until the HUD is shown again.
#[derive(Clone, Debug, Default)]
pub struct PromptTracker {
    active: Option<ActivePrompt>,
    entered_at: Option<Instant>,
}

impl PromptTracker {
    pub fn reset(&mut self) {
        self.active = None;
        self.entered_at = None;
    }

    /// Updates the eligible prompt for the current playback position. Returns
    /// `true` when the active prompt changed (entered, switched, or left).
    pub fn update(&mut self, eligible: Option<ActivePrompt>, now: Instant) -> bool {
        if self.active == eligible {
            return false;
        }
        self.active = eligible;
        self.entered_at = eligible.map(|_| now);
        true
    }

    /// The prompt that should currently be rendered, if any.
    ///
    /// Visible while inside the segment whenever:
    /// - the HUD is visible, OR
    /// - the pointer is hovering the prompt button itself, OR
    /// - fewer than [`PROMPT_AUTO_HIDE`] seconds have elapsed since entering
    ///   the segment.
    ///
    /// Suppressed while `blocked` (e.g. episode picker open).
    pub fn visible(
        &self,
        now: Instant,
        hud_visible: bool,
        hovered: bool,
        blocked: bool,
    ) -> Option<ActivePrompt> {
        if blocked {
            return None;
        }
        let prompt = self.active?;
        if hud_visible || hovered {
            return Some(prompt);
        }
        let within_grace = self
            .entered_at
            .is_some_and(|t| now.saturating_duration_since(t) < PROMPT_AUTO_HIDE);
        within_grace.then_some(prompt)
    }

    /// Time remaining before the floating prompt auto-hides (while the HUD is
    /// hidden), or `None` if already expired or inactive.
    pub fn time_until_hide(&self, now: Instant) -> Option<Duration> {
        let entered = self.entered_at?;
        let elapsed = now.saturating_duration_since(entered);
        (elapsed < PROMPT_AUTO_HIDE).then(|| PROMPT_AUTO_HIDE - elapsed)
    }
}

/// Which prompt is eligible at playback position `pos` given `times`, `duration`,
/// whether there is a next episode, and whether the EOF Up Next card is open.
pub fn eligible_prompt(
    times: &SkipTimes,
    pos: f64,
    duration: f64,
    has_next_ep: bool,
    up_next_open: bool,
) -> Option<ActivePrompt> {
    if !pos.is_finite() || pos < 0.0 {
        return None;
    }
    if let Some(intro) = times.intro
        && intro.contains(pos)
    {
        return Some(ActivePrompt::SkipIntro);
    }
    if has_next_ep
        && !up_next_open
        && let Some(outro) = times.outro
        && pos >= outro.start
        && (duration <= 0.0 || pos <= duration + 1.0)
    {
        return Some(ActivePrompt::NextEpisode);
    }
    None
}

/// Whether playback should immediately jump past the opening right now.
///
/// Only fires once per episode when the opening prompt is eligible, the
/// auto-skip setting is enabled, and the intro segment is known.
pub fn should_auto_skip(
    eligible: Option<ActivePrompt>,
    enabled: bool,
    already_skipped: bool,
    intro_known: bool,
) -> bool {
    eligible == Some(ActivePrompt::SkipIntro) && enabled && !already_skipped && intro_known
}

/// AniSkip accepts only positive integer episode numbers.
pub fn aniskip_episode(ep: &str) -> Option<u32> {
    let trimmed = ep.trim();
    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    trimmed.parse::<u32>().ok().filter(|&n| n > 0)
}

/// Fetches AniSkip entries for `(mal_id, ep)`, cached in `api_cache`. Empty
/// on 404, non-integer episodes, or network errors with nothing cached. Call
/// off the UI thread.
///
/// Asks with `episodeLength=0`: given a length, AniSkip returns only times
/// submitted for (nearly) exactly that length, and providers' cuts of an
/// episode differ by tens of seconds. [`resolve`] picks the entry closest to
/// the stream's real duration locally, so the request needn't wait for mpv
/// to report it and one cache entry serves every provider.
pub fn fetch_aniskip(db: &Mutex<Db>, mal_id: i64, ep: &str) -> Vec<AniSkipEntry> {
    let Some(ep_num) = aniskip_episode(ep).filter(|_| mal_id > 0) else {
        return Vec::new();
    };
    let key = format!("{CACHE_KEY_PREFIX}:{mal_id}:{ep_num}");
    catalog::fetch_cached(db, &key, CACHE_TTL_SECS, || {
        fetch_aniskip_http(mal_id, ep_num)
    })
    .unwrap_or_default()
}

fn fetch_aniskip_http(mal_id: i64, ep_num: u32) -> Result<Vec<AniSkipEntry>> {
    let url = format!(
        "{ANISKIP_API}/{mal_id}/{ep_num}?types=op&types=ed&types=mixed-op&types=mixed-ed&episodeLength=0"
    );
    let mut resp = AGENT
        .get(&url)
        .header("Accept", "application/json")
        .call()
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status().as_u16();
    // "No skip times found" is a 404 with a JSON body.
    if status == 404 {
        return Ok(Vec::new());
    }
    if !(200..300).contains(&status) {
        bail!("AniSkip HTTP {status} for {url}");
    }
    let v: Value = resp
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_json()
        .with_context(|| format!("decoding JSON from {url}"))?;
    Ok(parse_aniskip(&v))
}

/// Parses an AniSkip v2 `/skip-times` JSON payload, dropping malformed or
/// out-of-range intervals.
pub fn parse_aniskip(v: &Value) -> Vec<AniSkipEntry> {
    if v["found"].as_bool() != Some(true) {
        return Vec::new();
    }
    let Some(results) = v["results"].as_array() else {
        return Vec::new();
    };
    results
        .iter()
        .filter_map(|item| {
            let kind = match item["skipType"].as_str()? {
                "op" => SkipKind::Op,
                "mixed-op" => SkipKind::MixedOp,
                "ed" => SkipKind::Ed,
                "mixed-ed" => SkipKind::MixedEd,
                _ => return None,
            };
            let interval = Segment {
                start: item["interval"]["startTime"].as_f64()?,
                end: item["interval"]["endTime"].as_f64()?,
            };
            if !interval.is_valid() {
                return None;
            }
            let episode_length = item["episodeLength"]
                .as_f64()
                .filter(|l| l.is_finite() && *l >= 0.0)
                .unwrap_or(0.0);
            Some(AniSkipEntry {
                kind,
                interval,
                episode_length,
            })
        })
        .collect()
}

/// Parses mpv's `chapter-list` JSON string (`[{"title":"...","time":12.3}, ...]`).
pub fn parse_chapters(json: &str) -> Vec<Chapter> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    let mut chapters: Vec<Chapter> = items
        .iter()
        .filter_map(|item| {
            let time = item["time"]
                .as_f64()
                .filter(|t| t.is_finite() && *t >= 0.0)?;
            let title = item["title"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_string();
            Some(Chapter { title, time })
        })
        .collect();
    chapters.sort_by(|a, b| a.time.total_cmp(&b.time));
    chapters
}

/// Resolves the opening and ending segments for an episode:
/// 1. AniSkip entries (choosing the best length match, preferring `op`/`ed`
///    over `mixed-op`/`mixed-ed`).
/// 2. Fallback to mpv chapters titled like Opening/OP/Intro and
///    Ending/ED/Outro/Credits.
/// 3. With no data: no Skip intro; Next episode only in the last ~90 s (or
///    last 15% for episodes shorter than 3 minutes).
pub fn resolve(aniskip: &[AniSkipEntry], chapters: &[Chapter], duration: f64) -> SkipTimes {
    let duration = if duration.is_finite() && duration > 0.0 {
        duration
    } else {
        0.0
    };
    let intro = pick_aniskip(aniskip, SkipKind::Op, SkipKind::MixedOp, duration)
        .or_else(|| chapter_segment(chapters, duration, is_opening_title, false));
    let outro = pick_aniskip(aniskip, SkipKind::Ed, SkipKind::MixedEd, duration)
        .or_else(|| chapter_segment(chapters, duration, is_ending_title, true))
        .or_else(|| fallback_outro(duration));
    SkipTimes { intro, outro }
}

fn pick_aniskip(
    entries: &[AniSkipEntry],
    primary: SkipKind,
    mixed: SkipKind,
    duration: f64,
) -> Option<Segment> {
    let mut best: Option<(bool, bool, f64, Segment)> = None;
    for entry in entries {
        if entry.kind != primary && entry.kind != mixed {
            continue;
        }
        if !entry.interval.is_valid() {
            continue;
        }
        if duration > 0.0 && entry.interval.start >= duration {
            continue;
        }
        let end = if duration > 0.0 {
            entry.interval.end.min(duration)
        } else {
            entry.interval.end
        };
        let seg = Segment {
            start: entry.interval.start,
            end,
        };
        if !seg.is_valid() {
            continue;
        }
        let length_diff = if duration > 0.0 && entry.episode_length > 0.0 {
            (entry.episode_length - duration).abs()
        } else {
            f64::INFINITY
        };
        let within_tol = length_diff <= LENGTH_MATCH_TOLERANCE_SECS;
        let is_primary = entry.kind == primary;
        let better = match best {
            None => true,
            Some((best_tol, best_primary, best_diff, _)) => {
                // Prefer entries matching this stream's duration, then primary
                // (op/ed) over mixed, then closest length.
                (within_tol, is_primary, -length_diff) > (best_tol, best_primary, -best_diff)
            }
        };
        if better {
            best = Some((within_tol, is_primary, length_diff, seg));
        }
    }
    best.map(|(_, _, _, seg)| seg)
}

fn chapter_segment(
    chapters: &[Chapter],
    duration: f64,
    matches: fn(&str) -> bool,
    prefer_last: bool,
) -> Option<Segment> {
    let index = if prefer_last {
        chapters.iter().rposition(|c| matches(&c.title))
    } else {
        chapters.iter().position(|c| matches(&c.title))
    }?;
    let start = chapters[index].time;
    let end = chapters.get(index + 1).map(|c| c.time).unwrap_or(duration);
    let seg = Segment { start, end };
    seg.is_valid().then_some(seg)
}

fn fallback_outro(duration: f64) -> Option<Segment> {
    if duration >= FALLBACK_MIN_FULL_DURATION {
        Some(Segment {
            start: (duration - FALLBACK_OUTRO_SECS).max(0.0),
            end: duration,
        })
    } else if duration >= FALLBACK_MIN_SHORT_DURATION {
        Some(Segment {
            start: duration * FALLBACK_SHORT_RATIO,
            end: duration,
        })
    } else {
        None
    }
}

/// Tokenizes a chapter title into lowercase ASCII words.
fn title_words(title: &str) -> Vec<String> {
    title
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_ascii_lowercase())
        .collect()
}

pub fn is_opening_title(title: &str) -> bool {
    let words = title_words(title);
    if words.is_empty() {
        return false;
    }
    words.iter().any(|w| {
        matches!(w.as_str(), "opening" | "intro" | "op")
            || w.strip_prefix("op")
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    })
}

pub fn is_ending_title(title: &str) -> bool {
    let words = title_words(title);
    if words.is_empty() {
        return false;
    }
    words.iter().any(|w| {
        matches!(w.as_str(), "ending" | "outro" | "credits" | "ed")
            || w.strip_prefix("ed")
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parse_aniskip_real_shape_and_invalid_entries() {
        let payload = json!({
            "found": true,
            "results": [
                {
                    "interval": { "startTime": 3.221, "endTime": 93.221 },
                    "skipType": "op",
                    "skipId": "c2cacbe5",
                    "episodeLength": 1559.949
                },
                {
                    "interval": { "startTime": 1417.135, "endTime": 1507.135 },
                    "skipType": "ed",
                    "skipId": "92965d0e",
                    "episodeLength": 1510.102
                },
                {
                    "interval": { "startTime": 58.713, "endTime": 148.713 },
                    "skipType": "mixed-op",
                    "skipId": "6d856a3b",
                    "episodeLength": 1470.0
                },
                {
                    "interval": { "startTime": 100.0, "endTime": 100.0 },
                    "skipType": "op",
                    "episodeLength": 1440.0
                },
                {
                    "interval": { "startTime": 10.0, "endTime": 40.0 },
                    "skipType": "recap",
                    "episodeLength": 1440.0
                }
            ],
            "message": "Successfully found skip times",
            "statusCode": 200
        });
        let entries = parse_aniskip(&payload);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].kind, SkipKind::Op);
        assert_eq!(
            entries[0].interval,
            Segment {
                start: 3.221,
                end: 93.221
            }
        );
        assert_eq!(entries[1].kind, SkipKind::Ed);
        assert_eq!(entries[2].kind, SkipKind::MixedOp);

        let not_found = json!({
            "found": false,
            "results": [],
            "message": "No skip times found",
            "statusCode": 404
        });
        assert!(parse_aniskip(&not_found).is_empty());
    }

    #[test]
    fn resolve_prefers_aniskip_matching_duration_then_primary() {
        let entries = vec![
            AniSkipEntry {
                kind: SkipKind::Op,
                interval: Segment {
                    start: 3.0,
                    end: 93.0,
                },
                episode_length: 1560.0,
            },
            AniSkipEntry {
                kind: SkipKind::MixedOp,
                interval: Segment {
                    start: 58.0,
                    end: 148.0,
                },
                episode_length: 1470.0,
            },
            AniSkipEntry {
                kind: SkipKind::Ed,
                interval: Segment {
                    start: 1380.0,
                    end: 1470.0,
                },
                episode_length: 1470.0,
            },
        ];

        // Stream is 1470 s: mixed-op matches its length within 30 s, op is 90 s off.
        let short = resolve(&entries, &[], 1470.0);
        assert_eq!(
            short.intro,
            Some(Segment {
                start: 58.0,
                end: 148.0
            })
        );
        assert_eq!(
            short.outro,
            Some(Segment {
                start: 1380.0,
                end: 1470.0
            })
        );

        // Stream is 1555 s: op matches its length.
        let long = resolve(&entries, &[], 1555.0);
        assert_eq!(
            long.intro,
            Some(Segment {
                start: 3.0,
                end: 93.0
            })
        );
    }

    #[test]
    fn resolve_falls_back_to_mpv_chapters_then_last_90s() {
        let chapters = parse_chapters(
            r#"[
                {"title":"Prologue","time":0.0},
                {"title":"Opening","time":45.0},
                {"title":"Part A","time":135.0},
                {"title":"Ending Credits","time":1330.0},
                {"title":"Preview","time":1420.0}
            ]"#,
        );
        let times = resolve(&[], &chapters, 1440.0);
        assert_eq!(
            times.intro,
            Some(Segment {
                start: 45.0,
                end: 135.0
            })
        );
        assert_eq!(
            times.outro,
            Some(Segment {
                start: 1330.0,
                end: 1420.0
            })
        );

        // No AniSkip, no chapters: no intro, Next Episode in last 90 s.
        let none = resolve(&[], &[], 1440.0);
        assert_eq!(none.intro, None);
        assert_eq!(
            none.outro,
            Some(Segment {
                start: 1350.0,
                end: 1440.0
            })
        );

        // Short episode (120 s): last 15%, not 90 s.
        let short = resolve(&[], &[], 120.0);
        assert_eq!(short.intro, None);
        assert_eq!(
            short.outro,
            Some(Segment {
                start: 102.0,
                end: 120.0
            })
        );
    }

    #[test]
    fn chapter_title_matching_avoids_false_positives() {
        for title in ["Opening", "OP", "OP1", "OP 2", "Intro", "Opening Theme"] {
            assert!(is_opening_title(title), "{title:?}");
        }
        for title in ["Prologue", "Stop the Clock", "Drop", "Part A", ""] {
            assert!(!is_opening_title(title), "{title:?}");
        }
        for title in ["Ending", "ED", "ED1", "Outro", "Credits", "End Credits"] {
            assert!(is_ending_title(title), "{title:?}");
        }
        for title in ["Red Sky", "Ended", "Episode 1", "Preview", ""] {
            assert!(!is_ending_title(title), "{title:?}");
        }
    }

    #[test]
    fn aniskip_episode_accepts_only_positive_integers() {
        assert_eq!(aniskip_episode("1"), Some(1));
        assert_eq!(aniskip_episode("012"), Some(12));
        assert_eq!(aniskip_episode("0"), None);
        assert_eq!(aniskip_episode("2.5"), None);
        assert_eq!(aniskip_episode("special"), None);
        assert_eq!(aniskip_episode(""), None);
    }

    #[test]
    fn eligible_prompt_only_during_intro_and_from_outro_start() {
        let times = SkipTimes {
            intro: Some(Segment {
                start: 30.0,
                end: 120.0,
            }),
            outro: Some(Segment {
                start: 1320.0,
                end: 1410.0,
            }),
        };
        assert_eq!(eligible_prompt(&times, 10.0, 1440.0, true, false), None);
        assert_eq!(
            eligible_prompt(&times, 30.0, 1440.0, true, false),
            Some(ActivePrompt::SkipIntro)
        );
        assert_eq!(
            eligible_prompt(&times, 119.9, 1440.0, true, false),
            Some(ActivePrompt::SkipIntro)
        );
        assert_eq!(eligible_prompt(&times, 120.0, 1440.0, true, false), None);
        assert_eq!(eligible_prompt(&times, 1319.9, 1440.0, true, false), None);
        // Next episode starts at outro.start and stays through the post-credit scene to EOF.
        assert_eq!(
            eligible_prompt(&times, 1320.0, 1440.0, true, false),
            Some(ActivePrompt::NextEpisode)
        );
        assert_eq!(
            eligible_prompt(&times, 1435.0, 1440.0, true, false),
            Some(ActivePrompt::NextEpisode)
        );
        // Suppressed when there is no next episode or Up Next card is open.
        assert_eq!(eligible_prompt(&times, 1350.0, 1440.0, false, false), None);
        assert_eq!(eligible_prompt(&times, 1440.0, 1440.0, true, true), None);
    }

    #[test]
    fn prompt_tracker_netflix_visibility_and_auto_hide() {
        let t0 = Instant::now();
        let mut tracker = PromptTracker::default();
        assert_eq!(tracker.visible(t0, false, false, false), None);

        // Entering intro shows even when HUD is hidden.
        assert!(tracker.update(Some(ActivePrompt::SkipIntro), t0));
        assert!(!tracker.update(Some(ActivePrompt::SkipIntro), t0 + Duration::from_secs(1)));
        assert_eq!(
            tracker.visible(t0 + Duration::from_secs(2), false, false, false),
            Some(ActivePrompt::SkipIntro)
        );
        assert_eq!(
            tracker.time_until_hide(t0 + Duration::from_secs(3)),
            Some(Duration::from_secs(5))
        );

        // After 8 s untouched with HUD hidden, it fades out.
        let after = t0 + PROMPT_AUTO_HIDE;
        assert_eq!(tracker.visible(after, false, false, false), None);
        assert_eq!(tracker.time_until_hide(after), None);

        // Still reachable whenever the HUD is shown or pointer hovers it.
        assert_eq!(
            tracker.visible(after, true, false, false),
            Some(ActivePrompt::SkipIntro)
        );
        assert_eq!(
            tracker.visible(after, false, true, false),
            Some(ActivePrompt::SkipIntro)
        );
        // Blocked (e.g. episode picker open) hides it even with HUD visible.
        assert_eq!(tracker.visible(after, true, true, true), None);

        // Leaving intro and re-entering resets the 8 s window.
        assert!(tracker.update(None, after + Duration::from_secs(1)));
        assert_eq!(
            tracker.visible(after + Duration::from_secs(1), true, false, false),
            None
        );
        let t1 = after + Duration::from_secs(2);
        assert!(tracker.update(Some(ActivePrompt::NextEpisode), t1));
        assert_eq!(
            tracker.visible(t1 + Duration::from_secs(1), false, false, false),
            Some(ActivePrompt::NextEpisode)
        );
    }

    #[test]
    fn aniskip_cache_serves_stored_entries_without_network() {
        let db = Mutex::new(Db::open_in_memory().unwrap());
        let cached = vec![AniSkipEntry {
            kind: SkipKind::Op,
            interval: Segment {
                start: 10.0,
                end: 100.0,
            },
            episode_length: 1440.0,
        }];
        db.lock()
            .unwrap()
            .cache_put(
                "aniskip:v1:52991:1",
                &serde_json::to_string(&cached).unwrap(),
            )
            .unwrap();
        assert_eq!(fetch_aniskip(&db, 52991, "1"), cached);
        // Invalid episode / MAL id returns empty immediately without hitting network.
        assert!(fetch_aniskip(&db, 0, "1").is_empty());
        assert!(fetch_aniskip(&db, 52991, "1.5").is_empty());
    }

    #[test]
    fn should_auto_skip_only_when_all_four_conditions_hold() {
        assert!(should_auto_skip(
            Some(ActivePrompt::SkipIntro),
            true,
            false,
            true
        ));

        // Missing any single prerequisite suppresses auto-skip.
        assert!(!should_auto_skip(None, true, false, true));
        assert!(!should_auto_skip(
            Some(ActivePrompt::NextEpisode),
            true,
            false,
            true
        ));
        assert!(!should_auto_skip(
            Some(ActivePrompt::SkipIntro),
            false,
            false,
            true
        ));
        assert!(!should_auto_skip(
            Some(ActivePrompt::SkipIntro),
            true,
            true,
            true
        ));
        assert!(!should_auto_skip(
            Some(ActivePrompt::SkipIntro),
            true,
            false,
            false
        ));

        // Exhaustive check across all 24 combinations: only the one valid
        // tuple triggers auto-skip, and NextEpisode never does.
        for eligible in [
            None,
            Some(ActivePrompt::SkipIntro),
            Some(ActivePrompt::NextEpisode),
        ] {
            for enabled in [false, true] {
                for already_skipped in [false, true] {
                    for intro_known in [false, true] {
                        let expected = eligible == Some(ActivePrompt::SkipIntro)
                            && enabled
                            && !already_skipped
                            && intro_known;
                        assert_eq!(
                            should_auto_skip(eligible, enabled, already_skipped, intro_known),
                            expected,
                            "eligible={eligible:?}, enabled={enabled}, already_skipped={already_skipped}, intro_known={intro_known}"
                        );
                    }
                }
            }
        }
    }
}
