use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use serde::{Deserialize, Serialize};

use crate::anicli::normalize_title;
use crate::db::{self, Db, lock};
use crate::history::HistoryEntry;
use crate::lists::{self, ListEntry};
use crate::model::{AniListMedia, Mode, Progress, ShowRecord, ShowSummary, valid_episode};
use crate::progress;

/// How many shows Continue Watching considers from each source.
const PROGRESS_SHOWS: usize = 20;
const HISTORY_ENTRIES: usize = 12;
/// "Watching" shows from MyAnimeList/AniList not played here.
const REMOTE_ENTRIES: usize = 8;
const MAX_ITEMS: usize = 20;
const CONTINUE_CONCURRENCY: usize = 6;

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

/// Snapshot of `provider_titles` indexed both by show id and normalized title
/// so `continue_items` reads the table once instead of once per history entry.
struct ProviderIndex {
    by_id: HashMap<String, String>,
    by_norm: HashMap<String, Vec<String>>,
}

impl ProviderIndex {
    fn load(db: &Mutex<Db>) -> Self {
        let rows = lock(db).provider_titles().unwrap_or_else(|err| {
            eprintln!("ani-gui: reading provider titles: {err:#}");
            Vec::new()
        });
        let mut by_id = HashMap::with_capacity(rows.len());
        let mut by_norm: HashMap<String, Vec<String>> = HashMap::with_capacity(rows.len());
        for (id, title) in rows {
            let norm = normalize_title(&title);
            if !norm.is_empty() {
                by_norm.entry(norm).or_default().push(id.clone());
            }
            by_id.insert(id, title);
        }
        ProviderIndex { by_id, by_norm }
    }

    fn record_title(&mut self, id: &str, title: &str) {
        if self.by_id.contains_key(id) {
            return;
        }
        let norm = normalize_title(title);
        if !norm.is_empty() {
            self.by_norm.entry(norm).or_default().push(id.to_string());
        }
        self.by_id.insert(id.to_string(), title.to_string());
    }

    fn show_titles(&self, show: &ShowSummary) -> Vec<String> {
        [
            Some(show.title.as_str()),
            show.english_title.as_deref(),
            self.by_id.get(&show.id).map(String::as_str),
        ]
        .into_iter()
        .flatten()
        .map(normalize_title)
        .filter(|t| !t.is_empty())
        .collect()
    }

    fn find_stored(&self, db: &Mutex<Db>, wanted: &str) -> Option<ShowRecord> {
        self.by_norm
            .get(wanted)?
            .iter()
            .find_map(|id| stored_show(db, id))
    }
}

/// Shows to pick up again, newest first: the app's own progress, then
/// ani-cli history entries (newest lines last in its file) for shows without
/// app progress, matched by title, then shows the user is watching on
/// MyAnimeList/AniList, at the episode after the list's progress. `mode`
/// applies to history and list entries, which
/// do not record one. `misses` holds (mode, normalized title) pairs no search
/// matched; keep it for the session so they are not searched again. Blocks on
/// `catalog`; run it off the UI thread.
#[allow(dead_code)]
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
    let mut providers = ProviderIndex::load(db);
    let mut seen: HashSet<String> = recent.iter().map(|p| p.show_id.clone()).collect();
    let mut seen_titles: HashSet<String> = HashSet::new();
    let mut items = Vec::new();
    for latest in &recent {
        let Some(record) = stored_show(db, &latest.show_id) else {
            continue;
        };
        seen_titles.extend(providers.show_titles(&record.summary));
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
    for entry in history.iter().rev().take(HISTORY_ENTRIES) {
        let wanted = normalize_title(history_title(&entry.title));
        if wanted.is_empty() || seen_titles.contains(&wanted) {
            continue;
        }
        let Some(record) = match_history(db, &mut providers, catalog, entry, mode, misses) else {
            continue;
        };
        seen_titles.extend(providers.show_titles(&record.summary));
        if !seen.insert(record.summary.id.clone()) {
            continue;
        }
        let episodes = catalog.episodes(&record.summary, mode);
        let Some(ep) = progress::history_continue(&entry.ep, &episodes) else {
            continue;
        };
        items.push(item_for(db, catalog, record, mode, ep, 0.0, 0.0));
    }
    for entry in lists::watching(db).iter().take(REMOTE_ENTRIES) {
        if items.len() >= MAX_ITEMS {
            break;
        }
        let Some(record) = match_remote(db, catalog, entry, mode, misses) else {
            continue;
        };
        if !seen.insert(record.summary.id.clone())
            || providers
                .show_titles(&record.summary)
                .iter()
                .any(|t| seen_titles.contains(t))
        {
            continue;
        }
        let episodes = catalog.episodes(&record.summary, mode);
        let next = match entry.progress {
            0 => episodes.first().cloned(),
            n => progress::next_after(&n.to_string(), &episodes),
        };
        if let Some(ep) = next {
            items.push(item_for(db, catalog, record, mode, ep, 0.0, 0.0));
        }
    }
    items.truncate(MAX_ITEMS);
    items
}

/// Runs `work` over `inputs` across up to `CONTINUE_CONCURRENCY` scoped
/// threads, preserving input order in the returned `Vec`.
fn par_map<T, R, F>(inputs: &[T], work: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    if inputs.len() <= 1 {
        return inputs.iter().map(work).collect();
    }
    let workers = CONTINUE_CONCURRENCY.min(inputs.len());
    let next = AtomicUsize::new(0);
    let results: Vec<Mutex<Option<R>>> = (0..inputs.len()).map(|_| Mutex::new(None)).collect();
    thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= inputs.len() {
                        break;
                    }
                    let out = work(&inputs[i]);
                    *results[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(out);
                }
            });
        }
    });
    results
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .unwrap_or_else(|e| e.into_inner())
                .expect("every slot filled by par_map")
        })
        .collect()
}

/// Concurrent version of [`continue_items`] for thread-safe catalogs:
/// resolves independent AllAnime searches, episode lists, and AniList media
/// lookups in parallel while preserving the exact same ordering and dedup
/// semantics.
pub fn continue_items_concurrent(
    db: &Mutex<Db>,
    history: &[HistoryEntry],
    mode: Mode,
    catalog: &(impl Catalog + Sync),
    misses: &mut HashSet<(Mode, String)>,
) -> Vec<ContinueItem> {
    let recent = lock(db)
        .recent_progress(PROGRESS_SHOWS)
        .unwrap_or_else(|err| {
            eprintln!("ani-gui: reading progress: {err:#}");
            Vec::new()
        });
    let mut providers = ProviderIndex::load(db);
    let mut seen: HashSet<String> = recent.iter().map(|p| p.show_id.clone()).collect();
    let mut seen_titles: HashSet<String> = HashSet::new();

    // Phase 1: recent in-app progress (fetch episodes + missing media concurrently).
    let recent_records: Vec<(&Progress, ShowRecord)> = recent
        .iter()
        .filter_map(|latest| {
            let record = stored_show(db, &latest.show_id)?;
            seen_titles.extend(providers.show_titles(&record.summary));
            Some((latest, record))
        })
        .collect();
    let mut items: Vec<ContinueItem> = par_map(&recent_records, |(latest, record)| {
        let episodes = catalog.episodes(&record.summary, latest.mode);
        let (ep, start) = progress::continue_point(latest, &episodes)?;
        let fraction = if ep == latest.ep {
            watched_fraction(latest)
        } else {
            0.0
        };
        Some(item_for(
            db,
            catalog,
            record.clone(),
            latest.mode,
            ep,
            start,
            fraction,
        ))
    })
    .into_iter()
    .flatten()
    .collect();

    // Phase 2: ani-cli history entries (search + episodes + media concurrently).
    let mut queued_titles: HashSet<String> = HashSet::new();
    let history_candidates: Vec<(&HistoryEntry, String, Option<ShowRecord>)> = history
        .iter()
        .rev()
        .take(HISTORY_ENTRIES)
        .filter_map(|entry| {
            let wanted = normalize_title(history_title(&entry.title));
            if wanted.is_empty()
                || seen_titles.contains(&wanted)
                || misses.contains(&(mode, wanted.clone()))
                || !queued_titles.insert(wanted.clone())
            {
                return None;
            }
            let stored = providers.find_stored(db, &wanted);
            Some((entry, wanted, stored))
        })
        .collect();

    enum HistoryOutcome {
        Matched {
            record: Box<ShowRecord>,
            title_to_save: Option<String>,
            item: Option<Box<ContinueItem>>,
        },
        Missed(String),
        Failed,
    }

    let history_outcomes = par_map(&history_candidates, |(entry, wanted, stored)| {
        let title = history_title(&entry.title);
        let (record, title_to_save) = match stored {
            Some(record) => (record.clone(), None),
            None => {
                let Ok(results) = catalog.search(title, mode).inspect_err(|err| {
                    eprintln!("ani-gui: searching AllAnime for {title:?}: {err:#}")
                }) else {
                    return HistoryOutcome::Failed;
                };
                let Some(found) = results.into_iter().find(|s| {
                    normalize_title(&s.title) == *wanted
                        || s.english_title
                            .as_deref()
                            .is_some_and(|t| normalize_title(t) == *wanted)
                }) else {
                    return HistoryOutcome::Missed(wanted.clone());
                };
                let record = {
                    let guard = lock(db);
                    match guard.get_show(&found.id) {
                        Ok(Some(record)) => record,
                        _ => {
                            let record = ShowRecord {
                                summary: found,
                                media: None,
                                updated_at: db::now(),
                            };
                            if let Err(err) = guard.upsert_show(&record) {
                                eprintln!("ani-gui: saving show {}: {err:#}", record.summary.id);
                            }
                            record
                        }
                    }
                };
                (record, Some(title.to_string()))
            }
        };
        let episodes = catalog.episodes(&record.summary, mode);
        let item = progress::history_continue(&entry.ep, &episodes)
            .map(|ep| Box::new(item_for(db, catalog, record.clone(), mode, ep, 0.0, 0.0)));
        HistoryOutcome::Matched {
            record: Box::new(record),
            title_to_save,
            item,
        }
    });

    for outcome in history_outcomes {
        match outcome {
            HistoryOutcome::Matched {
                record,
                title_to_save,
                item,
            } => {
                if let Some(title) = title_to_save {
                    let saved = {
                        let guard = lock(db);
                        match guard.get_provider_title(&record.summary.id) {
                            Ok(Some(_)) => Ok(()),
                            Ok(None) => guard.set_provider_title(&record.summary.id, &title),
                            Err(err) => Err(err),
                        }
                    };
                    if let Err(err) = saved {
                        eprintln!("ani-gui: saving provider title: {err:#}");
                    } else {
                        providers.record_title(&record.summary.id, &title);
                    }
                }
                seen_titles.extend(providers.show_titles(&record.summary));
                if !seen.insert(record.summary.id.clone()) {
                    continue;
                }
                if let Some(item) = item {
                    items.push(*item);
                }
            }
            HistoryOutcome::Missed(wanted) => {
                misses.insert((mode, wanted));
            }
            HistoryOutcome::Failed => {}
        }
    }

    // Phase 3: MyAnimeList / AniList "watching" entries.
    if items.len() < MAX_ITEMS {
        let remote_entries: Vec<ListEntry> = lists::watching(db)
            .into_iter()
            .take(REMOTE_ENTRIES)
            .collect();
        let remote_candidates: Vec<(&ListEntry, Option<ShowRecord>)> = remote_entries
            .iter()
            .filter_map(|entry| {
                let stored = lock(db).show_by_anilist_id(entry.anilist_id).ok().flatten();
                if stored.is_none() {
                    let wanted = normalize_title(&entry.title);
                    if wanted.is_empty() || misses.contains(&(mode, wanted)) {
                        return None;
                    }
                }
                Some((entry, stored))
            })
            .collect();

        enum RemoteOutcome {
            Matched(Box<ShowRecord>, Option<Box<ContinueItem>>),
            Missed(String),
            Failed,
        }

        let remote_outcomes = par_map(&remote_candidates, |(entry, stored)| {
            let record = match stored {
                Some(record) => record.clone(),
                None => {
                    let Ok(results) = catalog.search(&entry.title, mode).inspect_err(|err| {
                        eprintln!("ani-gui: searching AllAnime for {:?}: {err:#}", entry.title);
                    }) else {
                        return RemoteOutcome::Failed;
                    };
                    let Some(found) = results
                        .into_iter()
                        .find(|s| s.anilist_id == Some(entry.anilist_id))
                    else {
                        return RemoteOutcome::Missed(normalize_title(&entry.title));
                    };
                    let record = ShowRecord {
                        summary: found,
                        media: None,
                        updated_at: db::now(),
                    };
                    if let Err(err) = lock(db).upsert_show(&record) {
                        eprintln!("ani-gui: saving show {}: {err:#}", record.summary.id);
                    }
                    record
                }
            };
            let episodes = catalog.episodes(&record.summary, mode);
            let next = match entry.progress {
                0 => episodes.first().cloned(),
                n => progress::next_after(&n.to_string(), &episodes),
            };
            let item =
                next.map(|ep| Box::new(item_for(db, catalog, record.clone(), mode, ep, 0.0, 0.0)));
            RemoteOutcome::Matched(Box::new(record), item)
        });

        for outcome in remote_outcomes {
            if items.len() >= MAX_ITEMS {
                break;
            }
            match outcome {
                RemoteOutcome::Matched(record, item) => {
                    if !seen.insert(record.summary.id.clone())
                        || providers
                            .show_titles(&record.summary)
                            .iter()
                            .any(|t| seen_titles.contains(t))
                    {
                        continue;
                    }
                    if let Some(item) = item {
                        items.push(*item);
                    }
                }
                RemoteOutcome::Missed(wanted) => {
                    misses.insert((mode, wanted));
                }
                RemoteOutcome::Failed => {}
            }
        }
    }

    items.truncate(MAX_ITEMS);
    items
}

/// The AllAnime show for a MyAnimeList/AniList "watching" entry: one
/// stored with its AniList id, else a search result carrying that id.
#[allow(dead_code)]
fn match_remote(
    db: &Mutex<Db>,
    catalog: &impl Catalog,
    entry: &ListEntry,
    mode: Mode,
    misses: &mut HashSet<(Mode, String)>,
) -> Option<ShowRecord> {
    if let Ok(Some(record)) = lock(db).show_by_anilist_id(entry.anilist_id) {
        return Some(record);
    }
    let wanted = normalize_title(&entry.title);
    if wanted.is_empty() || misses.contains(&(mode, wanted.clone())) {
        return None;
    }
    let results = catalog
        .search(&entry.title, mode)
        .inspect_err(|err| eprintln!("ani-gui: searching AllAnime for {:?}: {err:#}", entry.title))
        .ok()?;
    let Some(found) = results
        .into_iter()
        .find(|s| s.anilist_id == Some(entry.anilist_id))
    else {
        misses.insert((mode, wanted));
        return None;
    };
    let record = ShowRecord {
        summary: found,
        media: None,
        updated_at: db::now(),
    };
    if let Err(err) = lock(db).upsert_show(&record) {
        eprintln!("ani-gui: saving show {}: {err:#}", record.summary.id);
    }
    Some(record)
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

/// The show a history entry is about: one whose remembered provider title
/// matches, else an AllAnime search result with an equal (normalized) title.
/// A searched show without a provider title gets the entry's title as one.
#[allow(dead_code)]
fn match_history(
    db: &Mutex<Db>,
    providers: &mut ProviderIndex,
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
    if let Some(record) = providers.find_stored(db, &wanted) {
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
    } else {
        providers.record_title(&record.summary.id, title);
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
    /// `progress` is this show's progress in `mode`. `remote_progress` is
    /// how many episodes the user's MyAnimeList/AniList list says they
    /// watched: episodes numbered up to it count as watched, and Play
    /// starts after it unless local progress is further along.
    pub fn build(
        show: &ShowSummary,
        media: Option<&AniListMedia>,
        episodes: Option<&[String]>,
        progress: &[Progress],
        remote_progress: Option<u32>,
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
        let remote = remote_progress.filter(|&n| n > 0);
        let remote_watched = |ep: &str| {
            remote.is_some_and(|n| {
                ep.parse::<f64>()
                    .is_ok_and(|e| e > 0.0 && e <= f64::from(n))
            })
        };
        let latest = progress.iter().max_by_key(|p| p.updated_at);
        let local_target = latest.and_then(|latest| progress::continue_point(latest, episodes));
        // The episode after the list's progress (None once it's all watched).
        let remote_target = remote.map(|n| progress::next_after(&n.to_string(), episodes));
        let play_target = match (local_target, remote_target) {
            // Local progress wins unless the list is already past it.
            (Some((ep, start)), Some(next)) if remote_watched(&ep) => {
                next.map(|ep| (ep, 0.0)).or(Some((ep, start)))
            }
            (Some(local), _) => Some(local),
            (None, Some(Some(next))) => Some((next, 0.0)),
            _ => None,
        }
        .or_else(|| Some((episodes[0].clone(), 0.0)));
        let play_label = play_target
            .as_ref()
            .map(|(ep, start)| play_label(ep, *start))
            .unwrap_or_default();
        let progress_by_ep: HashMap<&str, &Progress> =
            progress.iter().map(|p| (p.ep.as_str(), p)).collect();
        let mut thumbs_by_ep: HashMap<u64, (Option<&str>, &str)> = HashMap::new();
        if let Some(m) = media {
            for (title, thumb) in &m.episode_thumbs {
                let Some(rest) = title.strip_prefix("Episode ") else {
                    continue;
                };
                let (num, name) = match rest.split_once(" - ") {
                    Some((num, name)) => (num, Some(name.trim()).filter(|n| !n.is_empty())),
                    None => (rest, None),
                };
                if let Ok(n) = num.trim().parse::<f64>()
                    && !n.is_nan()
                {
                    thumbs_by_ep
                        .entry((n + 0.0).to_bits())
                        .or_insert((name, thumb.as_str()));
                }
            }
        }
        let rows = episodes
            .iter()
            .map(|ep| {
                let watched = progress_by_ep.get(ep.as_str()).copied();
                let meta = ep
                    .parse::<f64>()
                    .ok()
                    .filter(|n| !n.is_nan())
                    .and_then(|n| thumbs_by_ep.get(&(n + 0.0).to_bits()).copied());
                let on_list = remote_watched(ep);
                let completed = watched.is_some_and(progress::is_completed) || on_list;
                let fraction = if on_list {
                    1.0
                } else {
                    watched.map_or(0.0, watched_fraction)
                };
                EpisodeRow {
                    ep: ep.clone(),
                    title: meta.and_then(|(title, _)| title).map(str::to_string),
                    thumb: meta.map(|(_, thumb)| thumb.to_string()),
                    fraction,
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
        let model = ShowPageModel::build(&s, None, Some(&eps(10)), &watched, None, Mode::Sub);
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
        let model = ShowPageModel::build(&s, Some(&media), Some(&eps(3)), &[], None, Mode::Sub);
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
        let model = ShowPageModel::build(&s, None, Some(&[]), &[], None, Mode::Dub);
        assert_eq!(model.play_label, "No dub episodes");
        assert_eq!(model.play_target, None);
        assert_eq!(model.other_mode, Some(Mode::Sub));
        assert!(model.episodes.is_empty());
    }

    #[test]
    fn show_page_model_episode_list_failed() {
        let s = show("s1", "Frieren", None);
        let model = ShowPageModel::build(&s, None, None, &[], None, Mode::Sub);
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
        let model = ShowPageModel::build(&s, None, Some(&eps(6)), &watched, None, Mode::Sub);
        let starts: Vec<f64> = model.episodes.iter().map(|r| r.start).collect();
        assert_eq!(starts, [0.0, 100.0, 0.0, 0.0, 560.0, 0.0]);
    }

    #[test]
    fn remote_progress_marks_episodes_watched() {
        let s = show("s1", "Frieren", None);
        let list: Vec<String> = ["1", "2", "2.5", "3", "4", "5"].map(String::from).into();
        // Watched up to 3 elsewhere, nothing played here.
        let model = ShowPageModel::build(&s, None, Some(&list), &[], Some(3), Mode::Sub);
        let done: Vec<bool> = model.episodes.iter().map(|r| r.completed).collect();
        assert_eq!(done, [true, true, true, true, false, false]);
        assert_eq!(model.episodes[0].fraction, 1.0);
        assert_eq!(model.episodes[4].fraction, 0.0);
        assert_eq!(model.play_target, Some(("4".into(), 0.0)));
        assert_eq!(model.play_label, "Play E4");
        // No local playback rows are invented for them.
        assert!(model.episodes.iter().all(|r| r.start == 0.0));
        assert_eq!(
            default_download_range(&model.episodes),
            Some(("4".into(), "5".into()))
        );
    }

    #[test]
    fn remote_progress_and_local_progress_combine() {
        let s = show("s1", "Frieren", None);
        // Local resume point already watched on the list: start after it.
        let behind = [progress("s1", "2", 300.0, 1400.0, 5)];
        let model = ShowPageModel::build(&s, None, Some(&eps(6)), &behind, Some(4), Mode::Sub);
        assert_eq!(model.play_target, Some(("5".into(), 0.0)));
        assert!(model.episodes[1].completed);
        assert_eq!(model.episodes[1].start, 0.0);
        // Local progress ahead of the list wins.
        let ahead = [progress("s1", "5", 300.0, 1400.0, 5)];
        let model = ShowPageModel::build(&s, None, Some(&eps(6)), &ahead, Some(2), Mode::Sub);
        assert_eq!(model.play_target, Some(("5".into(), 300.0)));
        assert!((model.episodes[4].fraction - 300.0 / 1400.0).abs() < 1e-6);
        assert!(!model.episodes[4].completed);
        // All watched on the list: Play E1 again, everything marked.
        let model = ShowPageModel::build(&s, None, Some(&eps(6)), &[], Some(6), Mode::Sub);
        assert_eq!(model.play_target, Some(("1".into(), 0.0)));
        assert!(model.episodes.iter().all(|r| r.completed));
        // Zero is no progress at all.
        let model = ShowPageModel::build(&s, None, Some(&eps(3)), &[], Some(0), Mode::Sub);
        assert!(model.episodes.iter().all(|r| !r.completed));
    }

    #[test]
    fn download_range_defaults_to_first_unwatched_through_last() {
        let s = show("s1", "Frieren", None);
        let watched = [
            progress("s1", "1", 1400.0, 1400.0, 5),
            progress("s1", "2", 1400.0, 1400.0, 6),
            progress("s1", "3", 300.0, 1400.0, 7),
        ];
        let model = ShowPageModel::build(&s, None, Some(&eps(6)), &watched, None, Mode::Sub);
        assert_eq!(
            default_download_range(&model.episodes),
            Some(("3".into(), "6".into()))
        );
        let all = [progress("s1", "1", 1400.0, 1400.0, 5)];
        let model = ShowPageModel::build(&s, None, Some(&eps(1)), &all, None, Mode::Sub);
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

    fn watching_entry(db: &Mutex<Db>, anilist_id: i64, title: &str, progress: u32) {
        use crate::lists::{ListStatus, RemoteEntry, Service};
        db.lock()
            .unwrap()
            .upsert_list_entry(&RemoteEntry {
                service: Service::AniList,
                remote_id: anilist_id,
                anilist_id: Some(anilist_id),
                mal_id: None,
                status: ListStatus::Watching,
                progress,
                score: None,
                episodes: Some(5),
                title: title.into(),
                cover_url: String::new(),
                updated_at: 10,
            })
            .unwrap();
    }

    #[test]
    fn continue_items_adds_list_shows_not_played_here() {
        let db = Mutex::new(Db::open_in_memory().unwrap());
        // Stored already (opened once): no search needed.
        let mut stored = show("aa1", "Sousou no Frieren", Some("Frieren"));
        stored.anilist_id = Some(154587);
        store(&db, &stored);
        watching_entry(&db, 154587, "Frieren", 2);
        // Not stored: found by searching, matched on the AniList id.
        let mut found = show("aa2", "Dandadan", None);
        found.anilist_id = Some(171018);
        watching_entry(&db, 171018, "Dandadan", 0);
        // Finished on the list: no next episode, no card.
        let mut done = show("aa3", "Done", None);
        done.anilist_id = Some(3);
        store(&db, &done);
        watching_entry(&db, 3, "Done", 5);
        let catalog = FakeCatalog {
            results: vec![show("wrong", "Dandadan", None), found],
            ..Default::default()
        };
        let mut misses = HashSet::new();
        let items = continue_items(&db, &[], Mode::Sub, &catalog, &mut misses);
        let got: Vec<(&str, &str, f64)> = items
            .iter()
            .map(|i| (i.show_id.as_str(), i.ep.as_str(), i.start))
            .collect();
        assert_eq!(got, [("aa1", "3", 0.0), ("aa2", "1", 0.0)]);
        assert_eq!(*catalog.searches.borrow(), ["Dandadan"]);
        assert!(db.lock().unwrap().get_show("aa2").unwrap().is_some());
    }

    #[test]
    fn continue_items_prefers_local_progress_over_the_list() {
        let db = Mutex::new(Db::open_in_memory().unwrap());
        let mut stored = show("aa1", "Sousou no Frieren", Some("Frieren"));
        stored.anilist_id = Some(154587);
        store(&db, &stored);
        db.lock()
            .unwrap()
            .save_progress(&progress("aa1", "4", 300.0, 1200.0, 50))
            .unwrap();
        watching_entry(&db, 154587, "Frieren", 1);
        watching_entry(&db, 99, "Nowhere", 1);
        let catalog = FakeCatalog::default();
        let mut misses = HashSet::new();
        let items = continue_items(&db, &[], Mode::Sub, &catalog, &mut misses);
        assert_eq!(items.len(), 1);
        assert_eq!((items[0].ep.as_str(), items[0].start), ("4", 300.0));
        // The unmatched list show is searched once, then remembered.
        continue_items(&db, &[], Mode::Sub, &catalog, &mut misses);
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
