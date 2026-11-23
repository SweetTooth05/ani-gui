use std::sync::{Mutex, MutexGuard, PoisonError};

use anyhow::Result;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::db::{self, Db};
use crate::model::{Airing, AniListMedia, Mode, ShowSummary};
use crate::{allanime, anilist};

const TTL_SECS: i64 = 6 * 60 * 60;
const TRENDING_COUNT: usize = 20;
const WEEK_SECS: i64 = 7 * 24 * 60 * 60;

pub fn trending(db: &Mutex<Db>) -> Vec<AniListMedia> {
    cached(db, "anilist:trending", || anilist::trending(TRENDING_COUNT)).unwrap_or_default()
}

/// Airings from now until a week ahead (a stale cache may include past ones).
pub fn schedule_week(db: &Mutex<Db>) -> Vec<Airing> {
    cached(db, "anilist:schedule", || {
        let now = db::now();
        anilist::schedule(now, now + WEEK_SECS)
    })
    .unwrap_or_default()
}

pub fn media(db: &Mutex<Db>, id: i64) -> Option<AniListMedia> {
    cached(db, &format!("anilist:media:{id}"), || anilist::media(id))
}

pub fn for_you(db: &Mutex<Db>, seed_ids: &[i64]) -> Vec<AniListMedia> {
    if seed_ids.is_empty() {
        return Vec::new();
    }
    let ids: Vec<String> = seed_ids.iter().map(i64::to_string).collect();
    cached(db, &format!("anilist:recs:{}", ids.join(",")), || {
        anilist::recommendations(seed_ids)
    })
    .unwrap_or_default()
}

/// The AllAnime show for an AniList entry: the result whose `anilist_id`
/// matches from a romaji-title search, then an English-title search, else the
/// first result.
pub fn resolve_allanime(media: &AniListMedia, mode: Mode) -> Result<Option<ShowSummary>> {
    let romaji = allanime::search(&media.title_romaji, mode)?;
    if let Some(show) = pick_match(&romaji, media.id) {
        return Ok(Some(show));
    }
    let english = match &media.title_english {
        Some(title) if *title != media.title_romaji => allanime::search(title, mode)?,
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

fn lock(db: &Mutex<Db>) -> MutexGuard<'_, Db> {
    db.lock().unwrap_or_else(PoisonError::into_inner)
}

fn cached<T, F>(db: &Mutex<Db>, key: &str, fetch: F) -> Option<T>
where
    T: Serialize + DeserializeOwned,
    F: FnOnce() -> Result<T>,
{
    let value = cached_or(db, key, TTL_SECS, || Ok(serde_json::to_value(fetch()?)?))?;
    serde_json::from_value(value)
        .inspect_err(|err| eprintln!("ani-gui: cached {key} is unreadable: {err}"))
        .ok()
}

/// Fresh cache entry, else `fetch` (stored on success), else the stale entry
/// when the fetch fails. The DB lock is not held while fetching.
fn cached_or<F: FnOnce() -> Result<Value>>(
    db: &Mutex<Db>,
    key: &str,
    ttl: i64,
    fetch: F,
) -> Option<Value> {
    let read = |json: Result<Option<String>>| {
        json.inspect_err(|err| eprintln!("ani-gui: reading cache {key}: {err:#}"))
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str(&json).ok())
    };
    if let Some(value) = read(lock(db).cache_get(key, ttl)) {
        return Some(value);
    }
    match fetch() {
        Ok(value) => {
            if let Err(err) = lock(db).cache_put(key, &value.to_string()) {
                eprintln!("ani-gui: writing cache {key}: {err:#}");
            }
            Some(value)
        }
        Err(err) => {
            eprintln!("ani-gui: fetching {key}: {err:#}");
            read(lock(db).cache_get_stale(key))
        }
    }
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;
    use serde_json::json;

    use super::*;

    fn db() -> Mutex<Db> {
        Mutex::new(Db::open_in_memory().unwrap())
    }

    #[test]
    fn cache_get_stale_allowed() {
        let db = db();
        assert_eq!(cached_or(&db, "k", 3600, || Err(anyhow!("offline"))), None);

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
            cached_or(&db, "k", 3600, || panic!("fresh cache must not refetch")),
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
}
