use std::sync::Mutex;

use anyhow::Result;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::db::{self, Db, lock};
use crate::model::{Airing, AniListMedia, Mode, ShowSummary};
use crate::{allanime, anilist};

const TTL_SECS: i64 = 6 * 60 * 60;
const EPISODES_TTL_SECS: i64 = 15 * 60;
const TRENDING_COUNT: usize = 20;
const WEEK_SECS: i64 = 7 * 24 * 60 * 60;

/// Bumped whenever the AniList queries ask for more fields. Old rows still
/// decode (new fields default), so without a new key they would be served
/// with those fields blank until they expire.
const ANILIST_KEY: &str = "anilist:v3";

pub fn trending(db: &Mutex<Db>) -> Vec<AniListMedia> {
    cached(db, &format!("{ANILIST_KEY}:trending"), || {
        anilist::trending(TRENDING_COUNT)
    })
    .unwrap_or_default()
}

/// One page of a Search listing (`anilist::Browse`), cached like the other
/// AniList lists.
pub fn browse(
    db: &Mutex<Db>,
    filters: &anilist::Browse,
    page: u32,
) -> Result<(Vec<AniListMedia>, bool)> {
    let key = serde_json::to_string(&filters.variables(page))?;
    fetch_cached(db, &format!("{ANILIST_KEY}:browse:{key}"), TTL_SECS, || {
        anilist::browse(filters, page)
    })
}

/// Airings from now until a week ahead (a stale cache may include past ones).
pub fn schedule_week(db: &Mutex<Db>) -> Vec<Airing> {
    cached(db, &format!("{ANILIST_KEY}:schedule"), || {
        let now = db::now();
        anilist::schedule(now, now + WEEK_SECS)
    })
    .unwrap_or_default()
}

/// Previously cached weekly schedule, fresh or stale, without network I/O.
pub fn schedule_week_cached(db: &Mutex<Db>) -> Option<Vec<Airing>> {
    cache_peek(db, &format!("{ANILIST_KEY}:schedule"))
}

pub fn media(db: &Mutex<Db>, id: i64) -> Option<AniListMedia> {
    cached(db, &format!("{ANILIST_KEY}:media:{id}"), || {
        anilist::media(id)
    })
}

/// Previously cached AniList media entry, fresh or stale, without network I/O.
pub fn media_cached(db: &Mutex<Db>, id: i64) -> Option<AniListMedia> {
    cache_peek(db, &format!("{ANILIST_KEY}:media:{id}"))
}

/// Episode list for an AllAnime show, cached in SQLite with a short TTL and
/// stale fallback when offline.
pub fn episodes(db: &Mutex<Db>, show_id: &str, mode: Mode) -> Result<Vec<String>> {
    let key = episodes_key(show_id, mode);
    fetch_cached(db, &key, EPISODES_TTL_SECS, || {
        allanime::episodes(show_id, mode)
    })
}

fn episodes_key(show_id: &str, mode: Mode) -> String {
    format!("allanime:eps:{show_id}:{}", mode.as_str())
}

/// Card data for `ids`, e.g. to dress AllAnime search results. Keyed by the
/// sorted ids, so the same result set hits the cache in any order.
pub fn cards(db: &Mutex<Db>, ids: &[i64]) -> Vec<AniListMedia> {
    let mut ids: Vec<i64> = ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    ids.truncate(50);
    if ids.is_empty() {
        return Vec::new();
    }
    let key: Vec<String> = ids.iter().map(i64::to_string).collect();
    cached(
        db,
        &format!("{ANILIST_KEY}:cards:{}", key.join(",")),
        || anilist::cards(&ids),
    )
    .unwrap_or_default()
}

pub fn for_you(db: &Mutex<Db>, seed_ids: &[i64]) -> Vec<AniListMedia> {
    if seed_ids.is_empty() {
        return Vec::new();
    }
    let ids: Vec<String> = seed_ids.iter().map(i64::to_string).collect();
    cached(db, &format!("{ANILIST_KEY}:recs:{}", ids.join(",")), || {
        anilist::recommendations(seed_ids)
    })
    .unwrap_or_default()
}

/// The AllAnime show for an AniList entry: checks the local `shows` table and
/// `api_cache` before hitting AllAnime's search API.
pub fn resolve_allanime_cached(
    db: &Mutex<Db>,
    media: &AniListMedia,
    mode: Mode,
) -> Result<Option<ShowSummary>> {
    if let Ok(Some(record)) = lock(db).show_by_anilist_id(media.id) {
        let count = match mode {
            Mode::Sub => record.summary.sub_count,
            Mode::Dub => record.summary.dub_count,
        };
        if count > 0 {
            return Ok(Some(record.summary));
        }
    }
    let key = format!("allanime:resolve:{}:{}", media.id, mode.as_str());
    fetch_cached(db, &key, TTL_SECS, || resolve_allanime(media, mode))
}

/// The AllAnime show for an AniList entry: the result whose `anilist_id`
/// matches from a romaji-title search, then an English-title search, else the
/// first result.
pub fn resolve_allanime(media: &AniListMedia, mode: Mode) -> Result<Option<ShowSummary>> {
    resolve_with(media, mode, allanime::search)
}

fn resolve_with<S>(media: &AniListMedia, mode: Mode, search: S) -> Result<Option<ShowSummary>>
where
    S: Fn(&str, Mode) -> Result<Vec<ShowSummary>>,
{
    let romaji = search(&media.title_romaji, mode)?;
    if let Some(show) = pick_match(&romaji, media.id) {
        return Ok(Some(show));
    }
    let english = match &media.title_english {
        Some(title) if *title != media.title_romaji => match search(title, mode) {
            Ok(found) => found,
            Err(err) if !romaji.is_empty() => {
                eprintln!("ani-gui: searching AllAnime for {title:?}: {err:#}");
                Vec::new()
            }
            Err(err) => return Err(err),
        },
        _ => Vec::new(),
    };
    if let Some(show) = pick_match(&english, media.id) {
        return Ok(Some(show));
    }
    Ok(romaji.into_iter().chain(english).next())
}

fn pick_match(results: &[ShowSummary], anilist_id: i64) -> Option<ShowSummary> {
    results
        .iter()
        .find(|s| s.anilist_id == Some(anilist_id))
        .cloned()
}

/// Reads any cached value (fresh or stale) for `key` without network I/O.
pub fn cache_peek<T: DeserializeOwned>(db: &Mutex<Db>, key: &str) -> Option<T> {
    lock(db)
        .cache_get_stale(key)
        .inspect_err(|err| eprintln!("ani-gui: reading cache {key}: {err:#}"))
        .ok()
        .flatten()
        .and_then(|json| {
            serde_json::from_str(&json)
                .inspect_err(|err| eprintln!("ani-gui: cached {key} is unreadable: {err}"))
                .ok()
        })
}

fn cached<T, F>(db: &Mutex<Db>, key: &str, fetch: F) -> Option<T>
where
    T: Serialize + DeserializeOwned,
    F: FnOnce() -> Result<T>,
{
    cached_or(db, key, TTL_SECS, fetch)
}

/// Fresh cache entry, else `fetch` (stored on success), else the stale entry
/// when the fetch fails. Entries that no longer decode as `T` count as
/// missing. The DB lock is not held while fetching.
fn cached_or<T, F>(db: &Mutex<Db>, key: &str, ttl: i64, fetch: F) -> Option<T>
where
    T: Serialize + DeserializeOwned,
    F: FnOnce() -> Result<T>,
{
    fetch_cached(db, key, ttl, fetch).ok()
}

/// `cached_or`, keeping the fetch error when there is no stale entry to
/// fall back on.
pub fn fetch_cached<T, F>(db: &Mutex<Db>, key: &str, ttl: i64, fetch: F) -> Result<T>
where
    T: Serialize + DeserializeOwned,
    F: FnOnce() -> Result<T>,
{
    let read = |json: Result<Option<String>>| {
        json.inspect_err(|err| eprintln!("ani-gui: reading cache {key}: {err:#}"))
            .ok()
            .flatten()
            .and_then(|json| {
                serde_json::from_str(&json)
                    .inspect_err(|err| eprintln!("ani-gui: cached {key} is unreadable: {err}"))
                    .ok()
            })
    };
    if let Some(value) = read(lock(db).cache_get(key, ttl)) {
        return Ok(value);
    }
    match fetch() {
        Ok(value) => {
            let stored = serde_json::to_string(&value)
                .map_err(anyhow::Error::from)
                .and_then(|json| lock(db).cache_put(key, &json));
            if let Err(err) = stored {
                eprintln!("ani-gui: writing cache {key}: {err:#}");
            }
            Ok(value)
        }
        Err(err) => {
            eprintln!("ani-gui: fetching {key}: {err:#}");
            read(lock(db).cache_get_stale(key)).ok_or(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;
    use serde_json::{Value, json};

    use super::*;

    fn db() -> Mutex<Db> {
        Mutex::new(Db::open_in_memory().unwrap())
    }

    #[test]
    fn cache_get_stale_allowed() {
        let db = db();
        assert_eq!(
            cached_or::<Value, _>(&db, "k", 3600, || Err(anyhow!("offline"))),
            None
        );

        db.lock()
            .unwrap()
            .cache_put("k", r#"{"old":true}"#)
            .unwrap();
        // A negative TTL makes every cached entry stale.
        assert_eq!(
            cached_or(&db, "k", -1, || Err(anyhow!("HTTP 429"))),
            Some(json!({ "old": true }))
        );
    }

    #[test]
    fn cached_or_uses_fresh_cache_then_stores_fetch() {
        let db = db();
        assert_eq!(
            cached_or(&db, "k", 3600, || Ok(json!([1, 2]))),
            Some(json!([1, 2]))
        );
        assert_eq!(
            db.lock().unwrap().cache_get_stale("k").unwrap().as_deref(),
            Some("[1,2]")
        );
        assert_eq!(
            cached_or::<Value, _>(&db, "k", 3600, || panic!("fresh cache must not refetch")),
            Some(json!([1, 2]))
        );
        assert_eq!(cached_or(&db, "k", -1, || Ok(json!([3]))), Some(json!([3])));
    }

    #[test]
    fn resolve_prefers_matching_anilist_id() {
        let show = |id: &str, anilist_id: Option<i64>| ShowSummary {
            id: id.into(),
            anilist_id,
            sub_count: 1,
            ..Default::default()
        };
        let results = [show("a", None), show("b", Some(7)), show("c", Some(9))];
        assert_eq!(pick_match(&results, 9).map(|s| s.id), Some("c".into()));
        assert_eq!(pick_match(&results, 8), None);
        assert_eq!(pick_match(&[], 9), None);
    }

    #[test]
    fn cached_refetches_unreadable_fresh_row() {
        let db = db();
        db.lock()
            .unwrap()
            .cache_put("anilist:trending", r#"{"old_shape":true}"#)
            .unwrap();
        let fresh = AniListMedia {
            id: 5,
            title_romaji: "Fresh".into(),
            ..Default::default()
        };
        let got: Option<Vec<AniListMedia>> =
            cached(&db, "anilist:trending", || Ok(vec![fresh.clone()]));
        assert_eq!(got, Some(vec![fresh.clone()]));
        let stored = db
            .lock()
            .unwrap()
            .cache_get_stale("anilist:trending")
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<AniListMedia>>(&stored.unwrap()).unwrap(),
            [fresh]
        );

        // An unreadable stale row is no fallback either.
        db.lock().unwrap().cache_put("k", "[1]").unwrap();
        let got: Option<Vec<AniListMedia>> = cached(&db, "k", || Err(anyhow!("offline")));
        assert_eq!(got, None);
    }

    #[test]
    fn resolve_falls_back_to_romaji_when_english_search_fails() {
        let media = AniListMedia {
            id: 154587,
            title_romaji: "Sousou no Frieren".into(),
            title_english: Some("Frieren: Beyond Journey's End".into()),
            ..Default::default()
        };
        let show = |id: &str| ShowSummary {
            id: id.into(),
            sub_count: 1,
            ..Default::default()
        };
        let search = |q: &str, _: Mode| -> Result<Vec<ShowSummary>> {
            if q.starts_with("Frieren") {
                Err(anyhow!("HTTP 503"))
            } else {
                Ok(vec![show("romaji-1"), show("romaji-2")])
            }
        };
        let got = resolve_with(&media, Mode::Sub, search).unwrap();
        assert_eq!(got.map(|s| s.id).as_deref(), Some("romaji-1"));

        let nothing = |q: &str, _: Mode| -> Result<Vec<ShowSummary>> {
            if q.starts_with("Frieren") {
                Err(anyhow!("HTTP 503"))
            } else {
                Ok(Vec::new())
            }
        };
        assert!(resolve_with(&media, Mode::Sub, nothing).is_err());
    }

    #[test]
    fn resolve_allanime_cached_hits_shows_table_before_network() {
        let db = db();
        let summary = ShowSummary {
            id: "stored-id".into(),
            title: "Frieren".into(),
            anilist_id: Some(154587),
            sub_count: 28,
            dub_count: 0,
            ..Default::default()
        };
        db.lock()
            .unwrap()
            .upsert_show(&crate::model::ShowRecord {
                summary: summary.clone(),
                media: None,
                updated_at: 100,
            })
            .unwrap();
        let media = AniListMedia {
            id: 154587,
            title_romaji: "Sousou no Frieren".into(),
            ..Default::default()
        };
        let resolved = resolve_allanime_cached(&db, &media, Mode::Sub).unwrap();
        assert_eq!(resolved, Some(summary));
    }

    #[test]
    fn cache_peek_reads_stored_values_without_fetching() {
        let db = db();
        assert_eq!(cache_peek::<Vec<String>>(&db, "allanime:eps:s1:sub"), None);
        db.lock()
            .unwrap()
            .cache_put("allanime:eps:s1:sub", r#"["1","2"]"#)
            .unwrap();
        assert_eq!(
            cache_peek::<Vec<String>>(&db, &episodes_key("s1", Mode::Sub)),
            Some(vec!["1".to_string(), "2".to_string()])
        );
    }
}
