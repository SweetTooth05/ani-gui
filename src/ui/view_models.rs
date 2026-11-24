use std::collections::HashSet;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::anicli::normalize_title;
use crate::db::{self, Db, lock};
use crate::history::HistoryEntry;
use crate::model::{AniListMedia, Mode, Progress, ShowRecord, ShowSummary, valid_episode};
use crate::progress;

/// How many shows Continue Watching considers from each source.
const PROGRESS_SHOWS: usize = 20;
const HISTORY_ENTRIES: usize = 12;
const MAX_ITEMS: usize = 20;

/// One Continue Watching card.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContinueItem {
    pub show_id: String,
    pub title: String,
    pub mode: Mode,
    pub ep: String,
    pub start: f64,
    /// How much of `ep` was watched (0 when it starts fresh).
    pub fraction: f32,
    pub cover_url: String,
    /// Landscape art: the episode's AniList thumbnail, else the show banner.
    pub backdrop_url: Option<String>,
    pub media: Option<AniListMedia>,
}

/// The network lookups `continue_items` needs, injectable for tests.
pub trait Catalog {
    fn search(&self, title: &str, mode: Mode) -> anyhow::Result<Vec<ShowSummary>>;
    fn episodes(&self, show: &ShowSummary, mode: Mode) -> Vec<String>;
    fn media(&self, anilist_id: i64) -> Option<AniListMedia>;
}

/// Shows to pick up again, newest first: the app's own progress, then
/// ani-cli history entries (newest lines last in its file) for shows without
/// app progress, matched by title. `mode` applies to history entries, which
/// do not record one. `misses` holds (mode, normalized title) pairs no search
/// matched; keep it for the session so they are not searched again. Blocks on
/// `catalog`; run it off the UI thread.
pub fn continue_items(
    db: &Mutex<Db>,
    history: &[HistoryEntry],
    mode: Mode,
    catalog: &impl Catalog,
    misses: &mut HashSet<(Mode, String)>,
) -> Vec<ContinueItem> {
    let recent = lock(db)
        .recent_progress(PROGRESS_SHOWS)
        .unwrap_or_else(|err| {
            eprintln!("ani-gui: reading progress: {err:#}");
            Vec::new()
        });
    let mut seen: HashSet<String> = recent.iter().map(|p| p.show_id.clone()).collect();
    let mut items = Vec::new();
    for latest in &recent {
        let Some(record) = stored_show(db, &latest.show_id) else {
            continue;
        };
        let episodes = catalog.episodes(&record.summary, latest.mode);
        let Some((ep, start)) = progress::continue_point(latest, &episodes) else {
            continue;
        };
        let fraction = if ep == latest.ep {
            watched_fraction(latest)
        } else {
            0.0
        };
        items.push(item_for(
            db,
            catalog,
            record,
            latest.mode,
            ep,
            start,
            fraction,
        ));
    }
    let mut seen_titles: HashSet<String> = HashSet::new();
    for id in &seen {
        if let Some(record) = stored_show(db, id) {
            seen_titles.extend(show_titles(db, &record.summary));
        }
    }
    for entry in history.iter().rev().take(HISTORY_ENTRIES) {
        let wanted = normalize_title(history_title(&entry.title));
        if wanted.is_empty() || seen_titles.contains(&wanted) {
            continue;
        }
        let Some(record) = match_history(db, catalog, entry, mode, misses) else {
            continue;
        };
        seen_titles.extend(show_titles(db, &record.summary));
        if !seen.insert(record.summary.id.clone()) {
            continue;
        }
        let episodes = catalog.episodes(&record.summary, mode);
        let Some(ep) = progress::history_continue(&entry.ep, &episodes) else {
            continue;
        };
        items.push(item_for(db, catalog, record, mode, ep, 0.0, 0.0));
    }
    items.truncate(MAX_ITEMS);
    items
}

fn item_for(
    db: &Mutex<Db>,
    catalog: &impl Catalog,
    mut record: ShowRecord,
    mode: Mode,
    ep: String,
    start: f64,
    fraction: f32,
) -> ContinueItem {
    if record.media.is_none()
        && let Some(media) = record.summary.anilist_id.and_then(|id| catalog.media(id))
    {
        record.media = Some(media);
        record.updated_at = db::now();
        if let Err(err) = lock(db).upsert_show(&record) {
            eprintln!("ani-gui: saving show {}: {err:#}", record.summary.id);
        }
    }
    let backdrop_url = record.media.as_ref().and_then(|m| {
        episode_meta(m, &ep)
            .map(|(_, thumb)| thumb.to_string())
            .or_else(|| m.banner_url.clone())
    });
    ContinueItem {
        show_id: record.summary.id.clone(),
        title: record.summary.display_title().to_string(),
        mode,
        ep,
        start,
        fraction,
        cover_url: record.summary.cover_url.clone(),
        backdrop_url,
        media: record.media,
    }
}

/// Normalized titles a history entry about `show` could carry.
fn show_titles(db: &Mutex<Db>, show: &ShowSummary) -> Vec<String> {
    let provider = lock(db).get_provider_title(&show.id).ok().flatten();
    [
        Some(show.title.as_str()),
        show.english_title.as_deref(),
        provider.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(normalize_title)
    .filter(|t| !t.is_empty())
    .collect()
}

/// The show a history entry is about: one whose remembered provider title
/// matches, else an AllAnime search result with an equal (normalized) title.
/// A searched show without a provider title gets the entry's title as one.
fn match_history(
    db: &Mutex<Db>,
    catalog: &impl Catalog,
    entry: &HistoryEntry,
    mode: Mode,
    misses: &mut HashSet<(Mode, String)>,
) -> Option<ShowRecord> {
    let title = history_title(&entry.title);
    let wanted = normalize_title(title);
    if wanted.is_empty() || misses.contains(&(mode, wanted.clone())) {
        return None;
    }
    let known = lock(db).provider_titles().unwrap_or_else(|err| {
        eprintln!("ani-gui: reading provider titles: {err:#}");
        Vec::new()
    });
    if let Some(record) = known
        .iter()
        .filter(|(_, t)| normalize_title(t) == wanted)
        .find_map(|(id, _)| stored_show(db, id))
    {
        return Some(record);
    }
    let results = catalog
        .search(title, mode)
        .inspect_err(|err| eprintln!("ani-gui: searching AllAnime for {title:?}: {err:#}"))
        .ok()?;
    let Some(found) = results.into_iter().find(|s| {
        normalize_title(&s.title) == wanted
            || s.english_title
                .as_deref()
                .is_some_and(|t| normalize_title(t) == wanted)
    }) else {
        misses.insert((mode, wanted));
        return None;
    };
    let db = lock(db);
    let record = match db.get_show(&found.id) {
        Ok(Some(record)) => record,
        _ => {
            let record = ShowRecord {
                summary: found,
                media: None,
                updated_at: db::now(),
            };
            if let Err(err) = db.upsert_show(&record) {
                eprintln!("ani-gui: saving show {}: {err:#}", record.summary.id);
            }
            record
        }
    };
    let saved = match db.get_provider_title(&record.summary.id) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => db.set_provider_title(&record.summary.id, title),
        Err(err) => Err(err),
    };
    if let Err(err) = saved {
        eprintln!("ani-gui: saving provider title: {err:#}");
    }
    Some(record)
}

/// Older ani-cli versions wrote "<title> (<n> episodes)".
fn history_title(title: &str) -> &str {
    title
        .rsplit_once(" (")
        .filter(|(_, tail)| {
            tail.strip_suffix(" episodes)")
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
        .map_or(title, |(head, _)| head)
        .trim()
}

fn stored_show(db: &Mutex<Db>, id: &str) -> Option<ShowRecord> {
    lock(db)
        .get_show(id)
        .inspect_err(|err| eprintln!("ani-gui: reading show {id}: {err:#}"))
        .ok()
        .flatten()
}

fn watched_fraction(p: &Progress) -> f32 {
    if p.duration > 0.0 {
        (p.position / p.duration).clamp(0.0, 1.0) as f32
    } else {
        0.0
    }
}

/// AniList's (title, thumbnail) for episode `ep`, from its
/// "Episode <n> - <title>" streaming entries; the title has the prefix removed.
pub fn episode_meta<'a>(media: &'a AniListMedia, ep: &str) -> Option<(Option<&'a str>, &'a str)> {
    let want: f64 = ep.parse().ok()?;
    media.episode_thumbs.iter().find_map(|(title, thumb)| {
        let rest = title.strip_prefix("Episode ")?;
        let (num, name) = match rest.split_once(" - ") {
            Some((num, name)) => (num, Some(name.trim()).filter(|n| !n.is_empty())),
            None => (rest, None),
        };
        (num.trim().parse::<f64>().ok()? == want).then_some((name, thumb.as_str()))
    })
}

#[derive(Clone, Debug, PartialEq)]
pub enum HeroSource {
    Continue(ContinueItem),
    Trending(AniListMedia),
}

#[derive(Clone, Debug, PartialEq)]
pub struct HeroModel {
    pub title: String,
    pub banner_url: Option<String>,
    pub cover_url: String,
    pub synopsis: Option<String>,
    pub genres: Vec<String>,
    pub year: Option<u16>,
    pub score: Option<u8>,
    pub play_label: String,
    pub source: HeroSource,
}

/// The first Continue Watching show, else the top trending one.
pub fn hero_pick(continue_items: &[ContinueItem], trending: &[AniListMedia]) -> Option<HeroModel> {
    if let Some(item) = continue_items.first() {
        let media = item.media.as_ref();
        return Some(HeroModel {
            title: item.title.clone(),
            banner_url: media.and_then(|m| m.banner_url.clone()),
            cover_url: item.cover_url.clone(),
            synopsis: media.and_then(|m| m.synopsis.clone()),
            genres: media.map(|m| m.genres.clone()).unwrap_or_default(),
            year: media.and_then(|m| m.year),
            score: media.and_then(|m| m.score),
            play_label: play_label(&item.ep, item.start),
            source: HeroSource::Continue(item.clone()),
        });
    }
    let media = trending.first()?;
    Some(HeroModel {
        title: media_title(media).to_string(),
        banner_url: media.banner_url.clone(),
        cover_url: media.cover_url.clone(),
        synopsis: media.synopsis.clone(),
        genres: media.genres.clone(),
        year: media.year,
        score: media.score,
        play_label: "Play E1".into(),
        source: HeroSource::Trending(media.clone()),
    })
}

pub fn media_title(media: &AniListMedia) -> &str {
    media
        .title_english
        .as_deref()
        .filter(|t| !t.is_empty())
        .unwrap_or(&media.title_romaji)
}

/// A synopsis as one paragraph for clamped previews, without AniList's
/// trailing "(Source: …)" credit.
pub fn blurb(synopsis: &str) -> String {
    synopsis
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("(Source:"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `text` cut at a word boundary to at most `max_chars` characters,
/// counting the "…" added when anything was cut. gpui's own ellipsis
/// measures clamped text as one long line, so wrapped previews lose their
/// last words without one; cutting first keeps the preview in its clamp.
pub fn clamp_words(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let cut: String = text.chars().take(keep).collect();
    // Back off to the last whole word unless the cut fell after one
    // (apostrophes are part of a word: "Frieren's").
    let mid_word = text
        .chars()
        .nth(keep)
        .is_some_and(|c| c.is_alphanumeric() || c == '\'' || c == '’');
    let words = match cut.rfind(char::is_whitespace) {
        Some(i) if mid_word => &cut[..i],
        _ => cut.as_str(),
    };
    let words = words.trim_end_matches(|c: char| c.is_whitespace() || c.is_ascii_punctuation());
    let words = if words.is_empty() {
        cut.as_str()
    } else {
        words
    };
    format!("{words}…")
}

/// AniList's media format for display ("TV_SHORT" is "TV short").
pub fn format_label(format: &str) -> String {
    match format {
        "TV_SHORT" => "TV short".into(),
        "MOVIE" => "Movie".into(),
        "SPECIAL" => "Special".into(),
        "MUSIC" => "Music".into(),
        other => other.to_string(),
    }
}

pub fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Sub => "Sub",
        Mode::Dub => "Dub",
    }
}

pub fn play_label(ep: &str, start: f64) -> String {
    if start > 0.0 {
        format!("Resume E{ep}")
    } else {
        format!("Play E{ep}")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct EpisodeRow {
    pub ep: String,
    pub title: Option<String>,
    pub thumb: Option<String>,
    pub fraction: f32,
    pub completed: bool,
    /// Where playing this episode starts: its saved position unless finished.
    pub start: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ShowPageModel {
    pub play_label: String,
    /// Episode and start position for the Play/Resume button.
    pub play_target: Option<(String, f64)>,
    /// With no episodes in the chosen mode: the other mode, if it has some.
    pub other_mode: Option<Mode>,
    pub episodes: Vec<EpisodeRow>,
}

impl ShowPageModel {
    /// `progress` is this show's progress in `mode`.
    pub fn build(
        show: &ShowSummary,
        media: Option<&AniListMedia>,
        episodes: Option<&[String]>,
        progress: &[Progress],
        mode: Mode,
    ) -> ShowPageModel {
        let Some(episodes) = episodes else {
            return ShowPageModel {
                play_label: "Couldn't load episodes".into(),
                ..ShowPageModel::default()
            };
        };
        if episodes.is_empty() {
            let other = match mode {
                Mode::Sub => Mode::Dub,
                Mode::Dub => Mode::Sub,
            };
            return ShowPageModel {
                play_label: format!("No {mode} episodes"),
                play_target: None,
                other_mode: (show.count(other) > 0).then_some(other),
                episodes: Vec::new(),
            };
        }
        let latest = progress.iter().max_by_key(|p| p.updated_at);
        let play_target = match latest {
            Some(latest) => progress::continue_point(latest, episodes)
                .or_else(|| Some((episodes[0].clone(), 0.0))),
            None => Some((episodes[0].clone(), 0.0)),
        };
        let play_label = play_target
            .as_ref()
            .map(|(ep, start)| play_label(ep, *start))
            .unwrap_or_default();
        let rows = episodes
            .iter()
            .map(|ep| {
                let watched = progress.iter().find(|p| p.ep == *ep);
                let meta = media.and_then(|m| episode_meta(m, ep));
                let completed = watched.is_some_and(progress::is_completed);
                EpisodeRow {
                    ep: ep.clone(),
                    title: meta.and_then(|(title, _)| title).map(str::to_string),
                    thumb: meta.map(|(_, thumb)| thumb.to_string()),
                    fraction: watched.map_or(0.0, watched_fraction),
                    completed,
                    start: watched.filter(|_| !completed).map_or(0.0, |p| p.position),
                }
            })
            .collect();
        ShowPageModel {
            play_label,
            play_target,
            other_mode: None,
            episodes: rows,
        }
    }
}

/// The Download-range control's starting bounds: the first unwatched
/// episode (else the first) through the last.
pub fn default_download_range(rows: &[EpisodeRow]) -> Option<(String, String)> {
    let last = rows.last()?;
    let first = rows.iter().find(|r| !r.completed).unwrap_or(&rows[0]);
    Some((first.ep.clone(), last.ep.clone()))
}

/// The episodes numbered from `from` to `to` (either order, inclusive), in
/// list order, or why there are none.
pub fn download_range(episodes: &[String], from: &str, to: &str) -> Result<Vec<String>, String> {
    let number = |s: &str| {
        let s = s.trim();
        valid_episode(s).then(|| s.parse::<f64>().ok()).flatten()
    };
    let (Some(a), Some(b)) = (number(from), number(to)) else {
        return Err("Enter episode numbers, like 1 and 12".into());
    };
    let (low, high) = if a <= b { (a, b) } else { (b, a) };
    let picked: Vec<String> = episodes
        .iter()
        .filter(|ep| ep.parse::<f64>().is_ok_and(|n| (low..=high).contains(&n)))
        .cloned()
        .collect();
    if picked.is_empty() {
        return Err(format!(
            "No episodes between {} and {}",
            from.trim(),
            to.trim()
        ));
    }
    Ok(picked)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::model::ShowRecord;

    fn eps(n: u32) -> Vec<String> {
        (1..=n).map(|e| e.to_string()).collect()
    }

    fn show(id: &str, title: &str, english: Option<&str>) -> ShowSummary {
        ShowSummary {
            id: id.into(),
            title: title.into(),
            english_title: english.map(str::to_string),
            cover_url: format!("https://img/{id}.jpg"),
            anilist_id: None,
            sub_count: 5,
            dub_count: 0,
        }
    }

    fn progress(show_id: &str, ep: &str, position: f64, duration: f64, at: i64) -> Progress {
        Progress {
            show_id: show_id.into(),
            mode: Mode::Sub,
            ep: ep.into(),
            position,
            duration,
            updated_at: at,
        }
    }

    fn item(title: &str, ep: &str, start: f64) -> ContinueItem {
        ContinueItem {
            show_id: "s1".into(),
            title: title.into(),
            mode: Mode::Sub,
            ep: ep.into(),
            start,
            fraction: 0.4,
            cover_url: "https://img/s1.jpg".into(),
            backdrop_url: None,
            media: Some(AniListMedia {
                id: 1,
                title_romaji: title.into(),
                banner_url: Some("https://img/banner.jpg".into()),
                synopsis: Some("An elf mage.".into()),
                ..Default::default()
            }),
        }
    }

    fn trending(title: &str) -> AniListMedia {
        AniListMedia {
            id: 9,
            title_romaji: format!("{title} (romaji)"),
            title_english: Some(title.into()),
            cover_url: "https://img/t.jpg".into(),
            ..Default::default()
        }
    }

    #[derive(Default)]
    struct FakeCatalog {
        results: Vec<ShowSummary>,
        offline: bool,
        searches: RefCell<Vec<String>>,
    }

    impl Catalog for FakeCatalog {
        fn search(&self, title: &str, _: Mode) -> anyhow::Result<Vec<ShowSummary>> {
            self.searches.borrow_mut().push(title.to_string());
            if self.offline {
                anyhow::bail!("offline");
            }
            Ok(self.results.clone())
        }
        fn episodes(&self, _: &ShowSummary, _: Mode) -> Vec<String> {
            eps(5)
        }
        fn media(&self, _: i64) -> Option<AniListMedia> {
            None
        }
    }

    fn history(ep: &str, title: &str) -> HistoryEntry {
        HistoryEntry {
            ep: ep.into(),
            id: "frieren-beyond-journeys-end-481".into(),
            title: title.into(),
        }
    }

    fn store(db: &Mutex<Db>, summary: &ShowSummary) {
        let record = ShowRecord {
            summary: summary.clone(),
            media: None,
            updated_at: 1,
        };
        db.lock().unwrap().upsert_show(&record).unwrap();
    }

    #[test]
    fn hero_prefers_continue_watching() {
        let hero = hero_pick(&[item("Frieren", "5", 480.0)], &[trending("Dandadan")]).unwrap();
        assert_eq!(hero.title, "Frieren");
        assert_eq!(hero.play_label, "Resume E5");
        assert_eq!(hero.banner_url.as_deref(), Some("https://img/banner.jpg"));
        assert!(matches!(hero.source, HeroSource::Continue(ref i) if i.ep == "5"));

        let fresh = hero_pick(&[item("Frieren", "6", 0.0)], &[]).unwrap();
        assert_eq!(fresh.play_label, "Play E6");
    }

    #[test]
    fn hero_falls_back_to_trending() {
        let hero = hero_pick(&[], &[trending("Dandadan"), trending("Other")]).unwrap();
        assert_eq!(hero.title, "Dandadan");
        assert!(matches!(hero.source, HeroSource::Trending(ref m) if m.id == 9));
        assert_eq!(hero_pick(&[], &[]), None);
    }

    #[test]
    fn blurb_flattens_paragraphs_and_drops_source_note() {
        assert_eq!(
            blurb("The adventure is over.\n\nA new one begins…\n\n(Source: Crunchyroll)"),
            "The adventure is over. A new one begins…"
        );
        assert_eq!(blurb("One line"), "One line");
    }

    #[test]
    fn clamp_words_cuts_at_a_word_with_ellipsis() {
        assert_eq!(clamp_words("Short enough", 40), "Short enough");
        assert_eq!(
            clamp_words("Frieren and her party defeated the Demon King", 24),
            "Frieren and her party…"
        );
        // Punctuation before the cut is dropped, never left before the "…".
        assert_eq!(clamp_words("One, two, three", 9), "One, two…");
        // Counts characters, not bytes.
        assert_eq!(clamp_words("Ōkami ōkami ōkami", 12), "Ōkami ōkami…");
        // An apostrophe belongs to its word: never "The Frieren…" from
        // "The Frieren's".
        assert_eq!(clamp_words("The Frieren's party", 12), "The…");
        assert_eq!(clamp_words("The Frieren’s party", 12), "The…");
        // A single word longer than the limit is cut inside it.
        assert_eq!(clamp_words("Supercalifragilistic", 8), "Superca…");
    }

    #[test]
    fn show_page_model_resume_label() {
        let s = show("s1", "Frieren", None);
        let watched = [
            progress("s1", "4", 1400.0, 1400.0, 10),
            progress("s1", "5", 560.0, 1400.0, 20),
        ];
        let model = ShowPageModel::build(&s, None, Some(&eps(10)), &watched, Mode::Sub);
        assert_eq!(model.play_label, "Resume E5");
        assert_eq!(model.play_target, Some(("5".into(), 560.0)));
        assert_eq!(model.episodes.len(), 10);
        assert!(model.episodes[3].completed);
        assert!((model.episodes[4].fraction - 0.4).abs() < 1e-6);
        assert!(!model.episodes[4].completed);
        assert_eq!(model.episodes[5].fraction, 0.0);
    }

    #[test]
    fn show_page_model_play_first() {
        let s = show("s1", "Frieren", None);
        let media = AniListMedia {
            episode_thumbs: vec![(
                "Episode 2 - It Didn't Have to Be Magic...".into(),
                "https://img/e2.jpg".into(),
            )],
            ..Default::default()
        };
        let model = ShowPageModel::build(&s, Some(&media), Some(&eps(3)), &[], Mode::Sub);
        assert_eq!(model.play_label, "Play E1");
        assert_eq!(model.play_target, Some(("1".into(), 0.0)));
        assert_eq!(model.episodes[0].thumb, None);
        assert_eq!(
            model.episodes[1].title.as_deref(),
            Some("It Didn't Have to Be Magic...")
        );
        assert_eq!(
            model.episodes[1].thumb.as_deref(),
            Some("https://img/e2.jpg")
        );
    }

    #[test]
    fn show_page_model_no_episodes_in_mode() {
        let s = show("s1", "Frieren", None);
        let model = ShowPageModel::build(&s, None, Some(&[]), &[], Mode::Dub);
        assert_eq!(model.play_label, "No dub episodes");
        assert_eq!(model.play_target, None);
        assert_eq!(model.other_mode, Some(Mode::Sub));
        assert!(model.episodes.is_empty());
    }

    #[test]
    fn show_page_model_episode_list_failed() {
        let s = show("s1", "Frieren", None);
        let model = ShowPageModel::build(&s, None, None, &[], Mode::Sub);
        assert_eq!(model.play_label, "Couldn't load episodes");
        assert_eq!(model.play_target, None);
        assert_eq!(model.other_mode, None);
        assert!(model.episodes.is_empty());
    }

    #[test]
    fn episode_rows_resume_where_left_off_unless_completed() {
        let s = show("s1", "Frieren", None);
        let watched = [
            progress("s1", "2", 100.0, 1400.0, 5),
            progress("s1", "4", 1400.0, 1400.0, 10),
            progress("s1", "5", 560.0, 1400.0, 20),
        ];
        let model = ShowPageModel::build(&s, None, Some(&eps(6)), &watched, Mode::Sub);
        let starts: Vec<f64> = model.episodes.iter().map(|r| r.start).collect();
        assert_eq!(starts, [0.0, 100.0, 0.0, 0.0, 560.0, 0.0]);
    }

    #[test]
    fn download_range_defaults_to_first_unwatched_through_last() {
        let s = show("s1", "Frieren", None);
        let watched = [
            progress("s1", "1", 1400.0, 1400.0, 5),
            progress("s1", "2", 1400.0, 1400.0, 6),
            progress("s1", "3", 300.0, 1400.0, 7),
        ];
        let model = ShowPageModel::build(&s, None, Some(&eps(6)), &watched, Mode::Sub);
        assert_eq!(
            default_download_range(&model.episodes),
            Some(("3".into(), "6".into()))
        );
        let all = [progress("s1", "1", 1400.0, 1400.0, 5)];
        let model = ShowPageModel::build(&s, None, Some(&eps(1)), &all, Mode::Sub);
        assert_eq!(
            default_download_range(&model.episodes),
            Some(("1".into(), "1".into()))
        );
        assert_eq!(default_download_range(&[]), None);
    }

    #[test]
    fn download_range_selects_episodes_between_bounds() {
        let list: Vec<String> = ["1", "2", "2.5", "3", "10"].map(String::from).into();
        let between = Ok(["2", "2.5", "3"].map(String::from).to_vec());
        assert_eq!(download_range(&list, "2", "3"), between);
        assert_eq!(download_range(&list, "3", "2"), between);
        assert_eq!(download_range(&list, " 10 ", "10"), Ok(vec!["10".into()]));
        assert_eq!(
            download_range(&list, "4", "9"),
            Err("No episodes between 4 and 9".into())
        );
        assert_eq!(
            download_range(&list, "first", "3"),
            Err("Enter episode numbers, like 1 and 12".into())
        );
        for bad in ["inf", "1e1", "-1", "NaN", "1\\|x"] {
            assert_eq!(
                download_range(&list, bad, "3"),
                Err("Enter episode numbers, like 1 and 12".into()),
                "{bad:?}"
            );
            assert!(download_range(&list, "1", bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn continue_items_merges_history_without_progress() {
        let db = Mutex::new(Db::open_in_memory().unwrap());
        let catalog = FakeCatalog {
            results: vec![
                show("other", "Frieren Movie", None),
                show(
                    "aa1",
                    "Sousou no Frieren",
                    Some("Frieren: Beyond Journey\u{2019}s End"),
                ),
            ],
            ..Default::default()
        };
        let items = continue_items(
            &db,
            &[history("3", "Frieren: Beyond Journey's End")],
            Mode::Sub,
            &catalog,
            &mut HashSet::new(),
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].show_id, "aa1");
        assert_eq!(items[0].ep, "4");
        assert_eq!(items[0].start, 0.0);
        assert_eq!(items[0].fraction, 0.0);
        // The history title becomes the show's provider title.
        let db = db.lock().unwrap();
        assert_eq!(
            db.get_provider_title("aa1").unwrap().as_deref(),
            Some("Frieren: Beyond Journey's End")
        );
        assert!(db.get_show("aa1").unwrap().is_some());
    }

    #[test]
    fn continue_items_matches_provider_title_first() {
        let db = Mutex::new(Db::open_in_memory().unwrap());
        store(&db, &show("known", "Sousou no Frieren", None));
        db.lock()
            .unwrap()
            .set_provider_title("known", "Frieren: Beyond Journey's End")
            .unwrap();
        let catalog = FakeCatalog {
            results: vec![show("wrong", "Frieren: Beyond Journey's End", None)],
            ..Default::default()
        };
        let items = continue_items(
            &db,
            &[history("1", "Frieren: Beyond Journey's End")],
            Mode::Sub,
            &catalog,
            &mut HashSet::new(),
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].show_id, "known");
        assert_eq!(items[0].ep, "2");
        assert!(catalog.searches.borrow().is_empty());
    }

    #[test]
    fn continue_items_prefers_app_progress_and_skips_unmatched() {
        let db = Mutex::new(Db::open_in_memory().unwrap());
        store(&db, &show("aa1", "Sousou no Frieren", Some("Frieren")));
        db.lock()
            .unwrap()
            .save_progress(&progress("aa1", "2", 300.0, 1200.0, 50))
            .unwrap();
        let catalog = FakeCatalog {
            results: vec![show("aa1", "Sousou no Frieren", Some("Frieren"))],
            ..Default::default()
        };
        let items = continue_items(
            &db,
            &[history("4", "Frieren"), history("1", "Nothing Like It")],
            Mode::Sub,
            &catalog,
            &mut HashSet::new(),
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].ep, "2");
        assert_eq!(items[0].start, 300.0);
        assert!((items[0].fraction - 0.25).abs() < 1e-6);
    }

    #[test]
    fn continue_items_skips_seen_shows_before_searching() {
        let db = Mutex::new(Db::open_in_memory().unwrap());
        store(&db, &show("aa1", "Sousou no Frieren", Some("Frieren")));
        db.lock()
            .unwrap()
            .save_progress(&progress("aa1", "2", 300.0, 1200.0, 50))
            .unwrap();
        let catalog = FakeCatalog {
            results: vec![show("aa1", "Sousou no Frieren", Some("Frieren"))],
            ..Default::default()
        };
        let items = continue_items(
            &db,
            &[history("4", "Frieren")],
            Mode::Sub,
            &catalog,
            &mut HashSet::new(),
        );
        assert_eq!(items.len(), 1);
        assert!(catalog.searches.borrow().is_empty());
    }

    #[test]
    fn continue_items_never_overwrites_a_provider_title() {
        let db = Mutex::new(Db::open_in_memory().unwrap());
        store(&db, &show("aa1", "Sousou no Frieren", None));
        db.lock()
            .unwrap()
            .set_provider_title("aa1", "Frieren (picked)")
            .unwrap();
        let catalog = FakeCatalog {
            results: vec![show("aa1", "Sousou no Frieren", None)],
            ..Default::default()
        };
        let items = continue_items(
            &db,
            &[history("2", "Sousou no Frieren")],
            Mode::Sub,
            &catalog,
            &mut HashSet::new(),
        );
        assert_eq!(items.len(), 1);
        assert_eq!(
            db.lock()
                .unwrap()
                .get_provider_title("aa1")
                .unwrap()
                .as_deref(),
            Some("Frieren (picked)")
        );
    }

    #[test]
    fn continue_items_remembers_misses() {
        let db = Mutex::new(Db::open_in_memory().unwrap());
        let catalog = FakeCatalog {
            results: vec![show("x", "Something Else", None)],
            ..Default::default()
        };
        let mut misses = HashSet::new();
        let history = [history("1", "Nothing Like It")];
        assert!(continue_items(&db, &history, Mode::Sub, &catalog, &mut misses).is_empty());
        assert!(continue_items(&db, &history, Mode::Sub, &catalog, &mut misses).is_empty());
        assert_eq!(catalog.searches.borrow().len(), 1);
    }

    #[test]
    fn continue_items_retries_searches_that_failed() {
        let db = Mutex::new(Db::open_in_memory().unwrap());
        let catalog = FakeCatalog {
            offline: true,
            ..Default::default()
        };
        let mut misses = HashSet::new();
        let history = [history("1", "Frieren")];
        continue_items(&db, &history, Mode::Sub, &catalog, &mut misses);
        continue_items(&db, &history, Mode::Sub, &catalog, &mut misses);
        assert!(misses.is_empty());
        assert_eq!(catalog.searches.borrow().len(), 2);
    }
}
