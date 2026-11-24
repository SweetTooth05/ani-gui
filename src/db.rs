use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, ValueRef};
use rusqlite::{Connection, OptionalExtension, Row, ToSql, params};

use crate::model::{DlStatus, DownloadRecord, Mode, Progress, Settings, ShowRecord, ShowSummary};

const SCHEMA_V1: &str = "
CREATE TABLE shows (
    id TEXT PRIMARY KEY,
    anilist_id INTEGER,
    title TEXT NOT NULL,
    english_title TEXT,
    cover_url TEXT NOT NULL,
    banner_url TEXT,
    synopsis TEXT,
    genres_json TEXT,
    score INTEGER,
    year INTEGER,
    format TEXT,
    status TEXT,
    sub_count INTEGER NOT NULL DEFAULT 0,
    dub_count INTEGER NOT NULL DEFAULT 0,
    media_json TEXT,
    updated_at INTEGER NOT NULL
);
CREATE TABLE progress (
    show_id TEXT NOT NULL,
    mode TEXT NOT NULL,
    ep TEXT NOT NULL,
    position REAL NOT NULL,
    duration REAL NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (show_id, mode, ep)
);
CREATE TABLE my_list (
    show_id TEXT PRIMARY KEY,
    added_at INTEGER NOT NULL
);
CREATE TABLE downloads (
    id TEXT PRIMARY KEY,
    show_id TEXT NOT NULL,
    title TEXT NOT NULL,
    ep TEXT NOT NULL,
    mode TEXT NOT NULL,
    quality TEXT NOT NULL,
    path TEXT,
    status TEXT NOT NULL,
    progress REAL NOT NULL DEFAULT 0,
    error TEXT,
    created_at INTEGER NOT NULL
);
CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE api_cache (
    key TEXT PRIMARY KEY,
    json TEXT NOT NULL,
    fetched_at INTEGER NOT NULL
);
";

/// ani-cli's own (provider) title per show, for its "Select anime" menu.
const SCHEMA_V2: &str = "
CREATE TABLE provider_titles (
    show_id TEXT PRIMARY KEY,
    title TEXT NOT NULL
);
";

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening database {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Db::init(conn)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Db> {
        Db::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Db> {
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (target, schema) in [(1, SCHEMA_V1), (2, SCHEMA_V2)] {
            if version < target {
                let tx = conn.transaction()?;
                tx.execute_batch(schema)?;
                tx.pragma_update(None, "user_version", target)?;
                tx.commit()?;
            }
        }
        Ok(Db { conn })
    }

    /// A record without `media` keeps the stored AniList data (`media_json` and
    /// the columns derived from it); a record with `media` replaces all of it.
    pub fn upsert_show(&self, show: &ShowRecord) -> Result<()> {
        let s = &show.summary;
        let m = show.media.as_ref();
        let genres_json = m.map(|m| serde_json::to_string(&m.genres)).transpose()?;
        let media_json = m.map(serde_json::to_string).transpose()?;
        self.conn.execute(
            "INSERT INTO shows (id, anilist_id, title, english_title, cover_url, banner_url,
                 synopsis, genres_json, score, year, format, status, sub_count, dub_count,
                 media_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
             ON CONFLICT(id) DO UPDATE SET
                 anilist_id = excluded.anilist_id, title = excluded.title,
                 english_title = excluded.english_title, cover_url = excluded.cover_url,
                 sub_count = excluded.sub_count, dub_count = excluded.dub_count,
                 updated_at = excluded.updated_at,
                 banner_url = IIF(excluded.media_json IS NULL, shows.banner_url, excluded.banner_url),
                 synopsis = IIF(excluded.media_json IS NULL, shows.synopsis, excluded.synopsis),
                 genres_json = IIF(excluded.media_json IS NULL, shows.genres_json, excluded.genres_json),
                 score = IIF(excluded.media_json IS NULL, shows.score, excluded.score),
                 year = IIF(excluded.media_json IS NULL, shows.year, excluded.year),
                 format = IIF(excluded.media_json IS NULL, shows.format, excluded.format),
                 status = IIF(excluded.media_json IS NULL, shows.status, excluded.status),
                 media_json = COALESCE(excluded.media_json, shows.media_json)",
            params![
                s.id,
                s.anilist_id,
                s.title,
                s.english_title,
                s.cover_url,
                m.and_then(|m| m.banner_url.as_deref()),
                m.and_then(|m| m.synopsis.as_deref()),
                genres_json,
                m.and_then(|m| m.score),
                m.and_then(|m| m.year),
                m.and_then(|m| m.format.as_deref()),
                m.and_then(|m| m.status.as_deref()),
                s.sub_count,
                s.dub_count,
                media_json,
                show.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_show(&self, id: &str) -> Result<Option<ShowRecord>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, title, english_title, cover_url, anilist_id, sub_count, dub_count,
                     media_json, updated_at
                 FROM shows WHERE id = ?1",
                [id],
                |r| {
                    let summary = ShowSummary {
                        id: r.get(0)?,
                        title: r.get(1)?,
                        english_title: r.get(2)?,
                        cover_url: r.get(3)?,
                        anilist_id: r.get(4)?,
                        sub_count: r.get(5)?,
                        dub_count: r.get(6)?,
                    };
                    let media_json: Option<String> = r.get(7)?;
                    Ok((summary, media_json, r.get::<_, i64>(8)?))
                },
            )
            .optional()?;
        let Some((summary, media_json, updated_at)) = row else {
            return Ok(None);
        };
        let media = media_json
            .map(|json| serde_json::from_str(&json))
            .transpose()
            .with_context(|| format!("decoding media_json for show {id}"))?;
        Ok(Some(ShowRecord {
            summary,
            media,
            updated_at,
        }))
    }

    pub fn save_progress(&self, p: &Progress) -> Result<()> {
        self.conn.execute(
            "INSERT INTO progress (show_id, mode, ep, position, duration, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(show_id, mode, ep) DO UPDATE SET
                 position = excluded.position, duration = excluded.duration,
                 updated_at = excluded.updated_at",
            params![
                p.show_id,
                p.mode,
                p.ep,
                p.position,
                p.duration,
                p.updated_at
            ],
        )?;
        Ok(())
    }

    pub fn progress_for_show(&self, show_id: &str, mode: Mode) -> Result<Vec<Progress>> {
        let mut stmt = self.conn.prepare(
            "SELECT show_id, mode, ep, position, duration, updated_at FROM progress
             WHERE show_id = ?1 AND mode = ?2
             ORDER BY CAST(ep AS REAL), ep",
        )?;
        let rows = stmt.query_map(params![show_id, mode], progress_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn recent_progress(&self, limit: usize) -> Result<Vec<Progress>> {
        // Ties on updated_at go to the most recently inserted row.
        let mut stmt = self.conn.prepare(
            "SELECT show_id, mode, ep, position, duration, updated_at FROM (
                 SELECT *, rowid AS rid, ROW_NUMBER() OVER (
                     PARTITION BY show_id ORDER BY updated_at DESC, rowid DESC
                 ) AS rn
                 FROM progress
             )
             WHERE rn = 1
             ORDER BY updated_at DESC, rid DESC LIMIT ?1",
        )?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = stmt.query_map([limit], progress_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn my_list_add(&self, show_id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO my_list (show_id, added_at) VALUES (?1, ?2)
             ON CONFLICT(show_id) DO NOTHING",
            params![show_id, now()],
        )?;
        Ok(())
    }

    pub fn my_list_remove(&self, show_id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM my_list WHERE show_id = ?1", [show_id])?;
        Ok(())
    }

    pub fn my_list_contains(&self, show_id: &str) -> Result<bool> {
        let found = self
            .conn
            .query_row(
                "SELECT 1 FROM my_list WHERE show_id = ?1",
                [show_id],
                |_| Ok(()),
            )
            .optional()?;
        Ok(found.is_some())
    }

    pub fn my_list(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT show_id FROM my_list ORDER BY added_at DESC, rowid DESC")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn upsert_download(&self, d: &DownloadRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO downloads (id, show_id, title, ep, mode, quality, path, status,
                 progress, error, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(id) DO UPDATE SET
                 show_id = excluded.show_id, title = excluded.title, ep = excluded.ep,
                 mode = excluded.mode, quality = excluded.quality, path = excluded.path,
                 status = excluded.status, progress = excluded.progress,
                 error = excluded.error, created_at = excluded.created_at",
            params![
                d.id,
                d.show_id,
                d.title,
                d.ep,
                d.mode,
                d.quality,
                d.path,
                d.status,
                f64::from(d.progress),
                d.error,
                d.created_at,
            ],
        )?;
        Ok(())
    }

    /// Newest first.
    pub fn list_downloads(&self) -> Result<Vec<DownloadRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, show_id, title, ep, mode, quality, path, status, progress, error,
                 created_at
             FROM downloads ORDER BY created_at DESC, rowid DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(DownloadRecord {
                id: r.get(0)?,
                show_id: r.get(1)?,
                title: r.get(2)?,
                ep: r.get(3)?,
                mode: r.get(4)?,
                quality: r.get(5)?,
                path: r.get(6)?,
                status: r.get(7)?,
                progress: r.get::<_, f64>(8)? as f32,
                error: r.get(9)?,
                created_at: r.get(10)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn delete_download(&self, id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM downloads WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Missing or unparsable keys fall back to `Settings::default()`.
    pub fn load_settings(&self) -> Result<Settings> {
        let mut settings = Settings::default();
        let mut stmt = self.conn.prepare("SELECT key, value FROM settings")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (key, value) = row?;
            match key.as_str() {
                "mode" => {
                    if let Ok(mode) = value.parse() {
                        settings.mode = mode;
                    }
                }
                "quality" if !value.is_empty() => settings.quality = value,
                "download_dir" if !value.is_empty() => settings.download_dir = PathBuf::from(value),
                "autoplay_next" => {
                    if let Ok(on) = value.parse() {
                        settings.autoplay_next = on;
                    }
                }
                _ => {}
            }
        }
        Ok(settings)
    }

    pub fn save_settings(&self, s: &Settings) -> Result<()> {
        let download_dir = s
            .download_dir
            .to_str()
            .context("download directory is not valid UTF-8")?;
        let entries = [
            ("mode", s.mode.as_str()),
            ("quality", s.quality.as_str()),
            ("download_dir", download_dir),
            (
                "autoplay_next",
                if s.autoplay_next { "true" } else { "false" },
            ),
        ];
        let tx = self.conn.unchecked_transaction()?;
        for (key, value) in entries {
            tx.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [key, value],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Returns the cached JSON only if it was fetched at most `max_age_secs` ago.
    pub fn cache_get(&self, key: &str, max_age_secs: i64) -> Result<Option<String>> {
        let json = self
            .conn
            .query_row(
                "SELECT json FROM api_cache WHERE key = ?1 AND fetched_at >= ?2",
                params![key, now().saturating_sub(max_age_secs)],
                |r| r.get(0),
            )
            .optional()?;
        Ok(json)
    }

    pub fn cache_get_stale(&self, key: &str) -> Result<Option<String>> {
        let json = self
            .conn
            .query_row("SELECT json FROM api_cache WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(json)
    }

    pub fn cache_put(&self, key: &str, json: &str) -> Result<()> {
        self.cache_put_at(key, json, now())
    }

    pub fn get_provider_title(&self, show_id: &str) -> Result<Option<String>> {
        let title = self
            .conn
            .query_row(
                "SELECT title FROM provider_titles WHERE show_id = ?1",
                [show_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(title)
    }

    /// Every remembered (show_id, provider title) pair.
    pub fn provider_titles(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT show_id, title FROM provider_titles ORDER BY rowid")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_provider_title(&self, show_id: &str, title: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO provider_titles (show_id, title) VALUES (?1, ?2)
             ON CONFLICT(show_id) DO UPDATE SET title = excluded.title",
            [show_id, title],
        )?;
        Ok(())
    }

    pub fn clear_provider_title(&self, show_id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM provider_titles WHERE show_id = ?1", [show_id])?;
        Ok(())
    }

    fn cache_put_at(&self, key: &str, json: &str, fetched_at: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO api_cache (key, json, fetched_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET
                 json = excluded.json, fetched_at = excluded.fetched_at",
            params![key, json, fetched_at],
        )?;
        Ok(())
    }
}

/// Locks the shared database, carrying on after a panic elsewhere left the
/// lock poisoned (every write is a single statement or transaction).
pub fn lock(db: &Mutex<Db>) -> MutexGuard<'_, Db> {
    db.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Current time in unix seconds.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn progress_from_row(r: &Row<'_>) -> rusqlite::Result<Progress> {
    Ok(Progress {
        show_id: r.get(0)?,
        mode: r.get(1)?,
        ep: r.get(2)?,
        position: r.get(3)?,
        duration: r.get(4)?,
        updated_at: r.get(5)?,
    })
}

fn parse_text<T: FromStr<Err = anyhow::Error>>(value: ValueRef<'_>) -> FromSqlResult<T> {
    value
        .as_str()?
        .parse()
        .map_err(|e: anyhow::Error| FromSqlError::Other(e.into()))
}

impl ToSql for Mode {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.as_str().into())
    }
}

impl FromSql for Mode {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        parse_text(value)
    }
}

impl ToSql for DlStatus {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.as_str().into())
    }
}

impl FromSql for DlStatus {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        parse_text(value)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::model::{AniListMedia, DlStatus, ShowSummary};

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    fn progress(show_id: &str, ep: &str, position: f64, updated_at: i64) -> Progress {
        Progress {
            show_id: show_id.into(),
            mode: Mode::Sub,
            ep: ep.into(),
            position,
            duration: 1440.0,
            updated_at,
        }
    }

    #[test]
    fn migrate_sets_latest_user_version() {
        let db = db();
        let version: i64 = db
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2);
        for table in [
            "shows",
            "progress",
            "my_list",
            "downloads",
            "settings",
            "api_cache",
            "provider_titles",
        ] {
            let n: i64 = db
                .conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "missing table {table}");
        }
    }

    #[test]
    fn migrate_v1_to_v2_keeps_data() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute(
            "INSERT INTO my_list (show_id, added_at) VALUES ('a', 1)",
            [],
        )
        .unwrap();
        let db = Db::init(conn).unwrap();
        let version: i64 = db
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2);
        assert_eq!(db.my_list().unwrap(), ["a"]);
        db.set_provider_title("a", "Frieren").unwrap();
        assert_eq!(
            db.get_provider_title("a").unwrap().as_deref(),
            Some("Frieren")
        );
    }

    #[test]
    fn clear_provider_title_forgets_one_show() {
        let db = db();
        db.set_provider_title("a", "Wrong Show").unwrap();
        db.set_provider_title("b", "Other").unwrap();
        db.clear_provider_title("a").unwrap();
        assert_eq!(db.get_provider_title("a").unwrap(), None);
        assert_eq!(
            db.get_provider_title("b").unwrap().as_deref(),
            Some("Other")
        );
        // Clearing again, or a show without one, is fine.
        db.clear_provider_title("a").unwrap();
        db.clear_provider_title("missing").unwrap();
        db.set_provider_title("a", "Right Show").unwrap();
        assert_eq!(
            db.get_provider_title("a").unwrap().as_deref(),
            Some("Right Show")
        );
    }

    #[test]
    fn provider_title_roundtrip() {
        let db = db();
        assert_eq!(db.get_provider_title("ReHMC7TQnch3C6z8j").unwrap(), None);
        db.set_provider_title("ReHMC7TQnch3C6z8j", "Frieren: Beyond Journey's End")
            .unwrap();
        db.set_provider_title("other", "Other").unwrap();
        db.set_provider_title(
            "ReHMC7TQnch3C6z8j",
            "Frieren: Beyond Journey's End Season 2",
        )
        .unwrap();
        assert_eq!(
            db.get_provider_title("ReHMC7TQnch3C6z8j")
                .unwrap()
                .as_deref(),
            Some("Frieren: Beyond Journey's End Season 2")
        );
        assert_eq!(
            db.get_provider_title("other").unwrap().as_deref(),
            Some("Other")
        );
        assert_eq!(
            db.provider_titles().unwrap(),
            [
                (
                    "ReHMC7TQnch3C6z8j".to_string(),
                    "Frieren: Beyond Journey's End Season 2".to_string()
                ),
                ("other".to_string(), "Other".to_string()),
            ]
        );
    }

    #[test]
    fn open_file_creates_parent_and_reopens() {
        let dir = std::env::temp_dir().join(format!("ani-gui-db-test-{}", std::process::id()));
        let path = dir.join("nested").join("ani-gui.db");
        {
            let db = Db::open(&path).unwrap();
            db.my_list_add("a").unwrap();
            let mode: String = db
                .conn
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .unwrap();
            assert_eq!(mode, "wal");
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.my_list().unwrap(), vec!["a".to_string()]);
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn show_roundtrip() {
        let db = db();
        let record = ShowRecord {
            summary: ShowSummary {
                id: "ReooPAxPMsHM4KPMY".into(),
                title: "Sousou no Frieren".into(),
                english_title: Some("Frieren: Beyond Journey's End".into()),
                cover_url: "https://example.com/cover.jpg".into(),
                anilist_id: Some(154587),
                sub_count: 28,
                dub_count: 28,
            },
            media: Some(AniListMedia {
                id: 154587,
                title_romaji: "Sousou no Frieren".into(),
                title_english: Some("Frieren: Beyond Journey's End".into()),
                cover_url: "https://example.com/xl.jpg".into(),
                banner_url: Some("https://example.com/banner.jpg".into()),
                synopsis: Some("An elf mage outlives her party.".into()),
                genres: vec!["Adventure".into(), "Fantasy".into()],
                score: Some(91),
                year: Some(2023),
                format: Some("TV".into()),
                status: Some("FINISHED".into()),
                episodes: Some(28),
                next_airing: Some((5, 1_700_000_000)),
                episode_thumbs: vec![("Episode 1".into(), "https://example.com/e1.jpg".into())],
                ..Default::default()
            }),
            updated_at: 1_700_000_000,
        };
        assert_eq!(db.get_show("ReooPAxPMsHM4KPMY").unwrap(), None);
        db.upsert_show(&record).unwrap();
        assert_eq!(
            db.get_show("ReooPAxPMsHM4KPMY").unwrap(),
            Some(record.clone())
        );

        let updated = ShowRecord {
            summary: ShowSummary {
                dub_count: 12,
                english_title: None,
                ..record.summary.clone()
            },
            media: None,
            updated_at: 1_700_000_100,
        };
        db.upsert_show(&updated).unwrap();
        assert_eq!(
            db.get_show("ReooPAxPMsHM4KPMY").unwrap(),
            Some(ShowRecord {
                media: record.media,
                ..updated
            })
        );
    }

    #[test]
    fn upsert_show_without_media_keeps_existing_media() {
        let db = db();
        let media = AniListMedia {
            id: 154587,
            title_romaji: "Sousou no Frieren".into(),
            banner_url: Some("https://example.com/banner.jpg".into()),
            synopsis: Some("An elf mage outlives her party.".into()),
            genres: vec!["Fantasy".into()],
            score: Some(91),
            year: Some(2023),
            format: Some("TV".into()),
            status: Some("FINISHED".into()),
            ..Default::default()
        };
        let summary = ShowSummary {
            id: "s1".into(),
            title: "Sousou no Frieren".into(),
            sub_count: 28,
            ..Default::default()
        };
        db.upsert_show(&ShowRecord {
            summary: summary.clone(),
            media: Some(media.clone()),
            updated_at: 1,
        })
        .unwrap();

        let refreshed = ShowSummary {
            dub_count: 12,
            ..summary.clone()
        };
        db.upsert_show(&ShowRecord {
            summary: refreshed.clone(),
            media: None,
            updated_at: 2,
        })
        .unwrap();
        assert_eq!(
            db.get_show("s1").unwrap(),
            Some(ShowRecord {
                summary: refreshed.clone(),
                media: Some(media.clone()),
                updated_at: 2,
            })
        );
        let columns = |db: &Db| {
            db.conn
                .query_row(
                    "SELECT banner_url, synopsis, genres_json, score, year, format, status
                     FROM shows WHERE id = 's1'",
                    [],
                    |r| {
                        Ok((
                            r.get::<_, Option<String>>(0)?,
                            r.get::<_, Option<String>>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, Option<u8>>(3)?,
                            r.get::<_, Option<u16>>(4)?,
                            r.get::<_, Option<String>>(5)?,
                            r.get::<_, Option<String>>(6)?,
                        ))
                    },
                )
                .unwrap()
        };
        assert_eq!(
            columns(&db),
            (
                media.banner_url.clone(),
                media.synopsis.clone(),
                Some(r#"["Fantasy"]"#.to_string()),
                Some(91),
                Some(2023),
                Some("TV".to_string()),
                Some("FINISHED".to_string()),
            )
        );

        // New media replaces the old columns wholesale, including cleared fields.
        let sparse = AniListMedia {
            id: 154587,
            title_romaji: "Sousou no Frieren".into(),
            ..Default::default()
        };
        db.upsert_show(&ShowRecord {
            summary: refreshed,
            media: Some(sparse.clone()),
            updated_at: 3,
        })
        .unwrap();
        assert_eq!(db.get_show("s1").unwrap().unwrap().media, Some(sparse));
        assert_eq!(
            columns(&db),
            (None, None, Some("[]".to_string()), None, None, None, None)
        );
    }

    #[test]
    fn progress_upsert_replaces_same_key() {
        let db = db();
        db.save_progress(&progress("s1", "1", 100.0, 10)).unwrap();
        db.save_progress(&progress("s1", "1", 200.0, 20)).unwrap();
        db.save_progress(&progress("s1", "2", 50.0, 30)).unwrap();
        db.save_progress(&Progress {
            mode: Mode::Dub,
            ..progress("s1", "1", 5.0, 40)
        })
        .unwrap();

        let sub = db.progress_for_show("s1", Mode::Sub).unwrap();
        assert_eq!(
            sub,
            vec![
                progress("s1", "1", 200.0, 20),
                progress("s1", "2", 50.0, 30)
            ]
        );
        assert_eq!(db.progress_for_show("s1", Mode::Dub).unwrap().len(), 1);
        assert!(db.progress_for_show("other", Mode::Sub).unwrap().is_empty());
    }

    #[test]
    fn recent_progress_latest_per_show() {
        let db = db();
        db.save_progress(&progress("a", "1", 10.0, 100)).unwrap();
        db.save_progress(&progress("b", "3", 30.0, 200)).unwrap();
        db.save_progress(&progress("a", "2", 20.0, 300)).unwrap();

        let recent = db.recent_progress(10).unwrap();
        assert_eq!(
            recent,
            vec![progress("a", "2", 20.0, 300), progress("b", "3", 30.0, 200)]
        );
        assert_eq!(
            db.recent_progress(1).unwrap(),
            vec![progress("a", "2", 20.0, 300)]
        );
    }

    #[test]
    fn recent_progress_tie_break_is_deterministic() {
        let db = db();
        db.save_progress(&progress("a", "1", 10.0, 100)).unwrap();
        db.save_progress(&progress("a", "2", 20.0, 100)).unwrap();
        db.save_progress(&progress("b", "1", 30.0, 100)).unwrap();
        db.save_progress(&progress("c", "1", 40.0, 50)).unwrap();

        let expected = vec![
            progress("b", "1", 30.0, 100),
            progress("a", "2", 20.0, 100),
            progress("c", "1", 40.0, 50),
        ];
        assert_eq!(db.recent_progress(10).unwrap(), expected);
        assert_eq!(db.recent_progress(2).unwrap(), expected[..2]);
    }

    #[test]
    fn my_list_add_remove_order() {
        let db = db();
        assert!(db.my_list().unwrap().is_empty());
        db.my_list_add("a").unwrap();
        db.my_list_add("b").unwrap();
        db.my_list_add("c").unwrap();
        db.my_list_add("a").unwrap();
        assert_eq!(db.my_list().unwrap(), vec!["c", "b", "a"]);
        assert!(db.my_list_contains("b").unwrap());

        db.my_list_remove("b").unwrap();
        assert!(!db.my_list_contains("b").unwrap());
        assert_eq!(db.my_list().unwrap(), vec!["c", "a"]);
        db.my_list_remove("missing").unwrap();
    }

    #[test]
    fn settings_defaults_then_roundtrip() {
        let db = db();
        let defaults = db.load_settings().unwrap();
        assert_eq!(defaults.mode, Mode::Sub);
        assert_eq!(defaults.quality, "best");
        assert!(defaults.autoplay_next);
        assert_eq!(defaults.download_dir, crate::paths::default_download_dir());

        let custom = Settings {
            mode: Mode::Dub,
            quality: "720".into(),
            download_dir: PathBuf::from("/tmp/anime"),
            autoplay_next: false,
        };
        db.save_settings(&custom).unwrap();
        assert_eq!(db.load_settings().unwrap(), custom);
    }

    #[test]
    fn cache_get_respects_age() {
        let db = db();
        assert_eq!(db.cache_get("k", 50).unwrap(), None);
        assert_eq!(db.cache_get_stale("k").unwrap(), None);

        db.cache_put_at("k", r#"{"old":true}"#, now() - 100)
            .unwrap();
        assert_eq!(db.cache_get("k", 50).unwrap(), None);
        assert_eq!(
            db.cache_get_stale("k").unwrap().as_deref(),
            Some(r#"{"old":true}"#)
        );

        db.cache_put("k", r#"{"new":true}"#).unwrap();
        assert_eq!(
            db.cache_get("k", 50).unwrap().as_deref(),
            Some(r#"{"new":true}"#)
        );
    }

    #[test]
    fn download_roundtrip_and_delete() {
        let db = db();
        let first = DownloadRecord {
            id: "d1".into(),
            show_id: "s1".into(),
            title: "Frieren".into(),
            ep: "1".into(),
            mode: Mode::Sub,
            quality: "best".into(),
            path: None,
            status: DlStatus::Queued,
            progress: 0.0,
            error: None,
            created_at: 100,
        };
        let second = DownloadRecord {
            id: "d2".into(),
            ep: "2".into(),
            mode: Mode::Dub,
            created_at: 200,
            ..first.clone()
        };
        db.upsert_download(&first).unwrap();
        db.upsert_download(&second).unwrap();

        let failed = DownloadRecord {
            status: DlStatus::Failed,
            progress: 42.5,
            path: Some("/tmp/Frieren Episode 1.mp4".into()),
            error: Some("stalled".into()),
            ..first.clone()
        };
        db.upsert_download(&failed).unwrap();
        assert_eq!(db.list_downloads().unwrap(), vec![second.clone(), failed]);

        db.delete_download("d1").unwrap();
        assert_eq!(db.list_downloads().unwrap(), vec![second]);
    }
}
