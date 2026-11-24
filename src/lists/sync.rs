//! The sync engine: imports, pushes with an offline retry queue, token
//! refresh, and signing in. Takes its database, token store and endpoints
//! as arguments so tests can run it against a local server.

use std::fmt;
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};

use super::http::HttpError;
use super::secrets::{TokenStore, Tokens};
use super::{
    ListStatus, PendingPush, RemoteEntry, Service, anilist_sync, entry_for, mal, oauth,
    progress_push, whole_episode,
};
use crate::db::{self, Db, lock};

/// Refresh MAL's access token this long before it expires.
const REFRESH_EARLY_SECS: i64 = 5 * 60;

/// Base URLs of every API used, so tests can swap in a local server.
#[derive(Clone, Debug)]
pub struct Endpoints {
    pub mal_oauth: String,
    pub mal_api: String,
    pub anilist: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints {
            mal_oauth: mal::OAUTH.into(),
            mal_api: mal::API.into(),
            anilist: anilist_sync::API.into(),
        }
    }
}

/// A service rejected its token, and the account was disconnected.
#[derive(Debug)]
pub struct AuthLost(pub Service);

impl fmt::Display for AuthLost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Your {} sign-in has expired or was revoked. Connect again in Settings.",
            self.0
        )
    }
}

impl std::error::Error for AuthLost {}

pub fn is_auth_lost(err: &anyhow::Error) -> bool {
    err.downcast_ref::<AuthLost>().is_some()
}

/// What finishing an episode did.
#[derive(Debug, Default)]
pub struct Completed {
    /// Each service a change was queued for, and whether it went through
    /// (failed ones stay queued).
    pub pushed: Vec<(Service, Result<()>)>,
    /// The show's title, when this completed it and it awaits a rating.
    pub finished: Option<String>,
}

/// A MAL sign-in waiting for the browser's redirect.
pub struct MalSignIn {
    pub listener: std::net::TcpListener,
    pub verifier: String,
    pub state: String,
    pub url: String,
}

pub struct Engine<'a> {
    pub db: &'a Mutex<Db>,
    pub store: &'a dyn TokenStore,
    pub ep: &'a Endpoints,
}

impl Engine<'_> {
    /// Connected services, MyAnimeList first.
    fn connected(&self) -> Result<Vec<Service>> {
        let mut services: Vec<Service> = lock(self.db)
            .list_accounts()?
            .into_iter()
            .filter(|a| a.connected())
            .map(|a| a.service)
            .collect();
        services.sort();
        Ok(services)
    }

    /// Disconnects `service` after its token was rejected.
    fn lose_auth(&self, service: Service) -> anyhow::Error {
        let lost = AuthLost(service);
        if let Err(err) = self.store.delete(service) {
            eprintln!("ani-gui: forgetting the {service} token: {err:#}");
        }
        if let Err(err) = lock(self.db).set_list_disconnected(service, Some(&lost.to_string())) {
            eprintln!("ani-gui: marking {service} disconnected: {err:#}");
        }
        lost.into()
    }

    fn tokens(&self, service: Service) -> Result<Tokens> {
        match self.store.get(service)? {
            Some(tokens) => Ok(tokens),
            None => Err(self.lose_auth(service)),
        }
    }

    fn refresh_mal(&self, tokens: Tokens) -> Result<Tokens> {
        let Some(refresh) = tokens.refresh.as_deref() else {
            return Err(self.lose_auth(Service::Mal));
        };
        let client_id = lock(self.db).list_account(Service::Mal)?.client_id;
        match mal::refresh(&self.ep.mal_oauth, &client_id, refresh, db::now()) {
            Ok(fresh) => {
                self.store.set(Service::Mal, &fresh)?;
                Ok(fresh)
            }
            Err(HttpError::Unauthorized | HttpError::Status(400, _)) => {
                Err(self.lose_auth(Service::Mal))
            }
            Err(err) => Err(anyhow!(err).context("refreshing the MyAnimeList sign-in")),
        }
    }

    /// Runs `call` with the service's access token. MAL's token is
    /// refreshed when it is about to expire or gets a 401; a 401 that
    /// survives that disconnects the account.
    fn authed<T>(
        &self,
        service: Service,
        call: impl Fn(&str) -> Result<T, HttpError>,
    ) -> Result<T> {
        let mut tokens = self.tokens(service)?;
        let expiring = tokens
            .expires_at
            .is_some_and(|at| at - REFRESH_EARLY_SECS <= db::now());
        if service == Service::Mal && expiring {
            tokens = self.refresh_mal(tokens)?;
        }
        match call(&tokens.access) {
            Err(HttpError::Unauthorized) if service == Service::Mal => {
                let fresh = self.refresh_mal(tokens)?;
                match call(&fresh.access) {
                    Err(HttpError::Unauthorized) => Err(self.lose_auth(service)),
                    other => other.map_err(anyhow::Error::from),
                }
            }
            Err(HttpError::Unauthorized) => Err(self.lose_auth(service)),
            other => other.map_err(anyhow::Error::from),
        }
    }

    /// Sends queued changes, then imports the whole list. Errors (other
    /// than a lost sign-in, which disconnects) are kept on the account for
    /// Settings to show.
    pub fn sync(&self, service: Service) -> Result<()> {
        if !lock(self.db).list_account(service)?.connected() {
            bail!("{service} isn't connected");
        }
        let result = self.push_pending(service).and_then(|()| {
            let entries = self.fetch(service)?;
            let db = lock(self.db);
            db.replace_list_entries(service, &entries)?;
            db.set_list_synced(service, db::now())
        });
        if let Err(err) = &result
            && !is_auth_lost(err)
            && let Err(e) = lock(self.db).set_list_error(service, Some(&format!("{err:#}")))
        {
            eprintln!("ani-gui: saving sync error: {e:#}");
        }
        result
    }

    fn fetch(&self, service: Service) -> Result<Vec<RemoteEntry>> {
        match service {
            Service::Mal => {
                let mut entries = self.authed(service, |t| mal::fetch_list(&self.ep.mal_api, t))?;
                self.map_ids(&mut entries)?;
                Ok(entries)
            }
            Service::AniList => {
                let user = lock(self.db)
                    .list_user_id(service)?
                    .context("AniList account has no user id; connect again")?;
                let entries = self.authed(service, |t| {
                    anilist_sync::fetch_list(&self.ep.anilist, t, user)
                })?;
                let db = lock(self.db);
                for e in &entries {
                    if let (Some(mal), Some(anilist)) = (e.mal_id, e.anilist_id) {
                        db.put_id_mapping(mal, Some(anilist))?;
                    }
                }
                Ok(entries)
            }
        }
    }

    /// Fills in AniList ids for MAL entries: cached ones first, the rest
    /// asked of AniList in batches (and cached, misses included). A failed
    /// lookup leaves entries unmapped until the next sync.
    fn map_ids(&self, entries: &mut [RemoteEntry]) -> Result<()> {
        let mal_ids: Vec<i64> = entries.iter().filter_map(|e| e.mal_id).collect();
        let mut known = lock(self.db).cached_anilist_ids(&mal_ids)?;
        let missing: Vec<i64> = mal_ids
            .iter()
            .copied()
            .filter(|id| !known.contains_key(id))
            .collect();
        for batch in missing.chunks(anilist_sync::MAP_BATCH) {
            match anilist_sync::map_mal_ids(&self.ep.anilist, batch) {
                Ok(pairs) => {
                    let db = lock(self.db);
                    for &mal in batch {
                        let found = pairs.iter().find(|p| p.0 == mal).map(|p| p.1);
                        db.put_id_mapping(mal, found)?;
                        known.insert(mal, found);
                    }
                }
                Err(err) => {
                    eprintln!("ani-gui: mapping MyAnimeList ids to AniList: {err}");
                    break;
                }
            }
        }
        for entry in entries.iter_mut() {
            entry.anilist_id = entry.mal_id.and_then(|m| known.get(&m).copied().flatten());
        }
        Ok(())
    }

    /// Sends the service's queued changes, oldest first, stopping at the
    /// first failure (the rest wait for the next sync).
    pub fn push_pending(&self, service: Service) -> Result<()> {
        let pending = lock(self.db).pending_list_pushes(service)?;
        for push in pending {
            match self.push_one(&push) {
                Ok(()) => lock(self.db).delete_list_push(service, push.anilist_id)?,
                Err(err) => {
                    if !is_auth_lost(&err) {
                        lock(self.db).list_push_failed(
                            service,
                            push.anilist_id,
                            &format!("{err:#}"),
                        )?;
                    }
                    return Err(err);
                }
            }
        }
        Ok(())
    }

    fn push_one(&self, p: &PendingPush) -> Result<()> {
        let mal_id = match p.service {
            Service::AniList => {
                self.authed(p.service, |t| {
                    anilist_sync::save_entry(
                        &self.ep.anilist,
                        t,
                        p.anilist_id,
                        p.status,
                        p.progress,
                        p.score,
                    )
                })?;
                None
            }
            Service::Mal => {
                let Some(mal_id) = self.mal_id(p.anilist_id)? else {
                    // Not on MAL at all: nothing will ever take this push.
                    eprintln!(
                        "ani-gui: AniList {} has no MyAnimeList entry; skipped",
                        p.anilist_id
                    );
                    return Ok(());
                };
                self.authed(p.service, |t| {
                    mal::update(&self.ep.mal_api, t, mal_id, p.status, p.progress, p.score)
                })?;
                Some(mal_id)
            }
        };
        self.apply_local(p, mal_id)
    }

    /// The MAL id of an AniList anime: known locally, else asked of AniList.
    fn mal_id(&self, anilist_id: i64) -> Result<Option<i64>> {
        if let Some(id) = lock(self.db).mal_id_for(anilist_id)? {
            return Ok(Some(id));
        }
        let found = anilist_sync::mal_id_of(&self.ep.anilist, anilist_id)
            .context("looking up the MyAnimeList id")?;
        if let Some(mal) = found {
            lock(self.db).put_id_mapping(mal, Some(anilist_id))?;
        }
        Ok(found)
    }

    /// Records a change in the local copy of the service's list, so the
    /// app shows it before the next import.
    fn apply_local(&self, p: &PendingPush, mal_id: Option<i64>) -> Result<()> {
        let db = lock(self.db);
        let existing = db.list_entry(p.service, p.anilist_id)?;
        let mal_id = mal_id
            .or_else(|| existing.as_ref().and_then(|e| e.mal_id))
            .or(db.mal_id_for(p.anilist_id)?);
        let remote_id = match p.service {
            Service::AniList => p.anilist_id,
            Service::Mal => match mal_id {
                Some(id) => id,
                None => return Ok(()),
            },
        };
        let (title, cover_url, episodes) = match &existing {
            Some(e) => (e.title.clone(), e.cover_url.clone(), e.episodes),
            None => match db.show_by_anilist_id(p.anilist_id)? {
                Some(show) => {
                    let media = show.media.as_ref();
                    let cover = media
                        .map(|m| m.cover_url.clone())
                        .filter(|c| !c.is_empty())
                        .unwrap_or(show.summary.cover_url.clone());
                    let title = media
                        .and_then(|m| m.title_english.clone())
                        .unwrap_or_else(|| show.summary.display_title().to_string());
                    (title, cover, media.and_then(|m| m.episodes))
                }
                None => (String::new(), String::new(), None),
            },
        };
        db.upsert_list_entry(&RemoteEntry {
            service: p.service,
            remote_id,
            anilist_id: Some(p.anilist_id),
            mal_id,
            status: p.status,
            progress: p.progress,
            score: p.score.or(existing.and_then(|e| e.score)),
            episodes,
            title,
            cover_url,
            updated_at: db::now(),
        })
    }

    /// Queues `push`, shows it locally, and returns it for sending.
    fn queue(&self, push: PendingPush) -> Result<()> {
        lock(self.db).queue_list_push(&push)?;
        self.apply_local(&push, None)
    }

    /// Episode `ep` of the stored show `show_id` was watched to the end:
    /// moves each connected service's progress up to it, and asks for a
    /// rating when that finished the show.
    pub fn episode_completed(&self, show_id: &str, ep: &str) -> Result<Completed> {
        let Some(ep) = whole_episode(ep) else {
            return Ok(Completed::default());
        };
        let services = self.connected()?;
        if services.is_empty() {
            return Ok(Completed::default());
        }
        let Some(record) = lock(self.db).get_show(show_id)? else {
            return Ok(Completed::default());
        };
        let media = record.media.as_ref();
        let Some(anilist_id) = record
            .summary
            .anilist_id
            .or(media.map(|m| m.id))
            .filter(|&id| id > 0)
        else {
            return Ok(Completed::default());
        };
        let merged = entry_for(self.db, anilist_id);
        let total = media
            .and_then(|m| m.episodes)
            .filter(|&n| n > 0)
            .or(merged.as_ref().and_then(|e| e.episodes));
        let mut queued = Vec::new();
        let mut finished = false;
        for service in services {
            let current = lock(self.db)
                .list_entry(service, anilist_id)?
                .map(|e| (e.status, e.progress));
            if let Some((status, progress)) = progress_push(current, ep, total) {
                self.queue(PendingPush {
                    service,
                    anilist_id,
                    status,
                    progress,
                    score: None,
                })?;
                queued.push(service);
                finished |= status == ListStatus::Completed;
            }
        }
        let pushed = queued
            .into_iter()
            .map(|s| (s, self.push_pending(s)))
            .collect();
        let rated = merged.is_some_and(|e| e.score.is_some());
        let finished = if finished && !rated {
            lock(self.db).add_rating_prompt(anilist_id)?;
            Some(record.summary.display_title().to_string())
        } else {
            None
        };
        Ok(Completed { pushed, finished })
    }

    /// Sets the score on every connected service, keeping each one's
    /// status and progress.
    pub fn rate(&self, anilist_id: i64, score: f32) -> Result<Vec<(Service, Result<()>)>> {
        let services = self.connected()?;
        if services.is_empty() {
            bail!("Connect MyAnimeList or AniList in Settings to rate shows");
        }
        let merged = entry_for(self.db, anilist_id);
        for &service in &services {
            let own = lock(self.db).list_entry(service, anilist_id)?;
            let (status, progress) = own
                .map(|e| (e.status, e.progress))
                .or(merged.as_ref().map(|m| (m.status, m.progress)))
                .unwrap_or((ListStatus::Completed, 0));
            self.queue(PendingPush {
                service,
                anilist_id,
                status,
                progress,
                score: Some(score),
            })?;
        }
        lock(self.db).clear_rating_prompt(anilist_id)?;
        Ok(services
            .into_iter()
            .map(|s| (s, self.push_pending(s)))
            .collect())
    }

    /// Checks a pasted AniList token and stores it in the keychain. Fails,
    /// connecting nothing, if there is no usable keychain.
    pub fn connect_anilist(&self, client_id: &str, pasted: &str) -> Result<String> {
        let token = anilist_sync::token_shape(pasted).context(
            "That doesn't look like an AniList token. Copy the whole token AniList showed you.",
        )?;
        let (user_id, name) =
            anilist_sync::viewer(&self.ep.anilist, token).map_err(|err| match err {
                HttpError::Unauthorized => {
                    anyhow!("AniList didn't accept that token. Connect again to get a new one.")
                }
                other => anyhow!("Couldn't check the token with AniList: {other}"),
            })?;
        self.store.set(
            Service::AniList,
            &Tokens {
                access: token.to_string(),
                refresh: None,
                expires_at: None,
            },
        )?;
        lock(self.db).set_list_connected(
            Service::AniList,
            client_id.trim(),
            user_id,
            &name,
            db::now(),
        )?;
        Ok(name)
    }

    /// Prepares a MAL sign-in: checks the client ID and the keychain (so
    /// the user isn't sent to the browser for nothing), takes the redirect
    /// port, and builds the authorize URL.
    pub fn begin_mal(&self, client_id: &str) -> Result<MalSignIn> {
        let client_id = client_id.trim();
        if client_id.is_empty()
            || client_id.len() > 128
            || !client_id.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            bail!("Paste the Client ID from your MyAnimeList API app first.");
        }
        self.store.get(Service::Mal)?;
        let verifier = oauth::pkce_verifier()?;
        let state = oauth::new_state()?;
        let listener = oauth::bind()?;
        lock(self.db).set_list_client_id(Service::Mal, client_id)?;
        let url = mal::authorize_url(&self.ep.mal_oauth, client_id, &verifier, &state);
        Ok(MalSignIn {
            listener,
            verifier,
            state,
            url,
        })
    }

    /// Swaps the redirect's code for tokens and stores them.
    pub fn finish_mal(&self, client_id: &str, code: &str, verifier: &str) -> Result<String> {
        let client_id = client_id.trim();
        let tokens = mal::exchange_code(&self.ep.mal_oauth, client_id, code, verifier, db::now())
            .map_err(|err| anyhow!("MyAnimeList didn't accept the sign-in: {err}"))?;
        let (user_id, name) = mal::me(&self.ep.mal_api, &tokens.access)
            .map_err(|err| anyhow!("Couldn't read your MyAnimeList profile: {err}"))?;
        self.store.set(Service::Mal, &tokens)?;
        lock(self.db).set_list_connected(Service::Mal, client_id, user_id, &name, db::now())?;
        Ok(name)
    }

    /// The whole MAL sign-in: browser, redirect (up to `oauth::WAIT_FOR`),
    /// token exchange. `busy` is taken only for the exchange, not while
    /// the user is in the browser.
    pub fn connect_mal<'g>(
        &self,
        client_id: &str,
        cancel: &AtomicBool,
        busy: impl FnOnce() -> MutexGuard<'g, ()>,
    ) -> Result<String> {
        let sign_in = self.begin_mal(client_id)?;
        oauth::open_browser(&sign_in.url)?;
        let code = oauth::wait_for_code(
            &sign_in.listener,
            &sign_in.state,
            Instant::now() + oauth::WAIT_FOR,
            cancel,
        )?;
        drop(sign_in.listener);
        let _busy = busy();
        self.finish_mal(client_id, &code, &sign_in.verifier)
    }

    /// Forgets the token and everything imported. The database is cleared
    /// even if the keychain can't be reached.
    pub fn disconnect(&self, service: Service) -> Result<()> {
        let deleted = self.store.delete(service);
        lock(self.db).forget_list_account(service)?;
        deleted
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::super::http::fake::Server;
    use super::super::secrets::MemoryStore;
    use super::*;
    use crate::model::{AniListMedia, ShowRecord, ShowSummary};

    fn endpoints(base: &str) -> Endpoints {
        Endpoints {
            mal_oauth: base.into(),
            mal_api: base.into(),
            anilist: base.into(),
        }
    }

    fn token(access: &str) -> Tokens {
        Tokens {
            access: access.into(),
            refresh: Some("REFRESH".into()),
            expires_at: Some(db::now() + 3600),
        }
    }

    /// A database with both accounts connected and a stored Frieren show.
    fn setup(store: &MemoryStore) -> Mutex<Db> {
        let db = Db::open_in_memory().unwrap();
        db.set_list_connected(Service::Mal, "CID", 1, "kev", 1)
            .unwrap();
        db.set_list_connected(Service::AniList, "123", 42, "kev", 1)
            .unwrap();
        db.upsert_show(&ShowRecord {
            summary: ShowSummary {
                id: "aa1".into(),
                title: "Sousou no Frieren".into(),
                english_title: Some("Frieren".into()),
                anilist_id: Some(154587),
                sub_count: 28,
                ..Default::default()
            },
            media: Some(AniListMedia {
                id: 154587,
                episodes: Some(28),
                cover_url: "https://img/f.jpg".into(),
                ..Default::default()
            }),
            updated_at: 1,
        })
        .unwrap();
        db.put_id_mapping(52991, Some(154587)).unwrap();
        store
            .tokens
            .lock()
            .unwrap()
            .insert(Service::Mal, token("MALTOK"));
        store
            .tokens
            .lock()
            .unwrap()
            .insert(Service::AniList, token("ALTOK"));
        Mutex::new(db)
    }

    fn body(seen: &super::super::http::fake::Seen) -> Value {
        serde_json::from_str(&seen.body).unwrap()
    }

    #[test]
    fn finishing_an_episode_pushes_to_both_services() {
        let store = MemoryStore::default();
        let db = setup(&store);
        let server = Server::start(vec![
            (200, r#"{"status":"watching"}"#.into()),
            (200, r#"{"data":{"SaveMediaListEntry":{"id":1}}}"#.into()),
        ]);
        let ep = endpoints(&server.base);
        let engine = Engine {
            db: &db,
            store: &store,
            ep: &ep,
        };
        let done = engine.episode_completed("aa1", "5").unwrap();
        assert_eq!(done.finished, None);
        assert!(done.pushed.iter().all(|(_, r)| r.is_ok()));
        let seen = server.finish();
        assert_eq!(seen[0].path, "/anime/52991/my_list_status");
        assert_eq!(seen[0].body, "status=watching&num_watched_episodes=5");
        assert_eq!(seen[0].header("authorization"), Some("Bearer MALTOK"));
        let vars = &body(&seen[1])["variables"];
        assert_eq!(
            vars,
            &json!({ "mediaId": 154587, "status": "CURRENT", "progress": 5 })
        );
        assert_eq!(seen[1].header("authorization"), Some("Bearer ALTOK"));

        {
            let db = db.lock().unwrap();
            assert!(db.pending_list_pushes(Service::Mal).unwrap().is_empty());
            let mal = db.list_entry(Service::Mal, 154587).unwrap().unwrap();
            assert_eq!(
                (mal.remote_id, mal.progress, mal.status),
                (52991, 5, ListStatus::Watching)
            );
            assert_eq!(mal.title, "Frieren");
        }

        // The same episode again, or an earlier one, sends nothing.
        let none = Server::start(vec![]);
        let ep = endpoints(&none.base);
        let engine = Engine {
            db: &db,
            store: &store,
            ep: &ep,
        };
        assert!(
            engine
                .episode_completed("aa1", "5")
                .unwrap()
                .pushed
                .is_empty()
        );
        assert!(
            engine
                .episode_completed("aa1", "2")
                .unwrap()
                .pushed
                .is_empty()
        );
        assert!(
            engine
                .episode_completed("aa1", "5.5")
                .unwrap()
                .pushed
                .is_empty()
        );
        assert!(
            engine
                .episode_completed("unknown", "9")
                .unwrap()
                .pushed
                .is_empty()
        );
        assert!(none.finish().is_empty());
    }

    #[test]
    fn the_final_episode_completes_and_asks_for_a_rating() {
        let store = MemoryStore::default();
        let db = setup(&store);
        let server = Server::start(vec![
            (200, "{}".into()),
            (200, r#"{"data":{"SaveMediaListEntry":{"id":1}}}"#.into()),
        ]);
        let ep = endpoints(&server.base);
        let engine = Engine {
            db: &db,
            store: &store,
            ep: &ep,
        };
        let done = engine.episode_completed("aa1", "28").unwrap();
        assert_eq!(done.finished.as_deref(), Some("Frieren"));
        let seen = server.finish();
        assert_eq!(seen[0].body, "status=completed&num_watched_episodes=28");
        assert_eq!(body(&seen[1])["variables"]["status"], "COMPLETED");
        assert!(db.lock().unwrap().rating_prompt(154587).unwrap());

        // Rating sends the score with the completed status, and clears
        // the prompt.
        let server = Server::start(vec![
            (200, "{}".into()),
            (200, r#"{"data":{"SaveMediaListEntry":{"id":1}}}"#.into()),
        ]);
        let ep = endpoints(&server.base);
        let engine = Engine {
            db: &db,
            store: &store,
            ep: &ep,
        };
        let results = engine.rate(154587, 9.0).unwrap();
        assert!(results.iter().all(|(_, r)| r.is_ok()));
        let seen = server.finish();
        assert_eq!(
            seen[0].body,
            "status=completed&num_watched_episodes=28&score=9"
        );
        assert_eq!(
            body(&seen[1])["variables"],
            json!({ "mediaId": 154587, "status": "COMPLETED", "progress": 28, "scoreRaw": 90 })
        );
        assert!(!db.lock().unwrap().rating_prompt(154587).unwrap());
        assert_eq!(entry_for(&db, 154587).unwrap().score, Some(9.0));
    }

    #[test]
    fn failed_pushes_wait_in_the_queue_for_the_next_sync() {
        let store = MemoryStore::default();
        let db = setup(&store);
        db.lock()
            .unwrap()
            .forget_list_account(Service::AniList)
            .unwrap();
        let server = Server::start(vec![(503, r#"{"message":"maintenance"}"#.into())]);
        let ep = endpoints(&server.base);
        let engine = Engine {
            db: &db,
            store: &store,
            ep: &ep,
        };
        let done = engine.episode_completed("aa1", "3").unwrap();
        assert_eq!(done.pushed.len(), 1);
        assert!(done.pushed[0].1.is_err());
        server.finish();
        assert_eq!(
            db.lock()
                .unwrap()
                .pending_list_pushes(Service::Mal)
                .unwrap()
                .len(),
            1
        );
        // Shown locally already.
        assert_eq!(entry_for(&db, 154587).unwrap().progress, 3);

        // The next sync sends it first, then imports.
        let list = json!({
            "data": [{ "node": { "id": 52991, "title": "Sousou no Frieren", "num_episodes": 28 },
                       "list_status": { "status": "watching", "num_episodes_watched": 3,
                                        "updated_at": "2024-03-23T10:15:00+00:00" } },
                     { "node": { "id": 5114, "title": "FMA:B" },
                       "list_status": { "status": "plan_to_watch" } }],
            "paging": {}
        });
        let server = Server::start(vec![
            (200, "{}".into()),
            (200, list.to_string()),
            (
                200,
                r#"{"data":{"Page":{"media":[{"id":5114,"idMal":5114}]}}}"#.into(),
            ),
        ]);
        let ep = endpoints(&server.base);
        let engine = Engine {
            db: &db,
            store: &store,
            ep: &ep,
        };
        engine.sync(Service::Mal).unwrap();
        let seen = server.finish();
        assert_eq!(seen[0].method, "PATCH");
        assert_eq!(seen[1].method, "GET");
        // Only the unmapped MAL id is looked up.
        assert_eq!(body(&seen[2])["variables"]["ids"], json!([5114]));
        let db = db.lock().unwrap();
        assert!(db.pending_list_pushes(Service::Mal).unwrap().is_empty());
        let account = db.list_account(Service::Mal).unwrap();
        assert!(account.last_sync.is_some() && account.last_error.is_none());
        let ids: Vec<Option<i64>> = db
            .list_entries()
            .unwrap()
            .iter()
            .map(|e| e.anilist_id)
            .collect();
        assert!(ids.contains(&Some(154587)) && ids.contains(&Some(5114)));
    }

    #[test]
    fn anilist_401_disconnects_the_account() {
        let store = MemoryStore::default();
        let db = setup(&store);
        let server = Server::start(vec![(
            400,
            r#"{"errors":[{"message":"Invalid token","status":400}]}"#.into(),
        )]);
        let ep = endpoints(&server.base);
        let engine = Engine {
            db: &db,
            store: &store,
            ep: &ep,
        };
        let err = engine.sync(Service::AniList).unwrap_err();
        assert!(is_auth_lost(&err), "{err:#}");
        server.finish();
        let account = db.lock().unwrap().list_account(Service::AniList).unwrap();
        assert!(!account.connected());
        assert!(account.last_error.unwrap().contains("Connect again"));
        assert!(store.get(Service::AniList).unwrap().is_none());
        // MAL is untouched.
        assert!(store.get(Service::Mal).unwrap().is_some());
    }

    #[test]
    fn mal_refreshes_an_expiring_token_and_after_a_401() {
        let store = MemoryStore::default();
        let db = setup(&store);
        store.tokens.lock().unwrap().insert(
            Service::Mal,
            Tokens {
                expires_at: Some(db::now() + 10),
                ..token("OLD")
            },
        );
        let server = Server::start(vec![
            (
                200,
                r#"{"access_token":"NEW","refresh_token":"R2","expires_in":3600}"#.into(),
            ),
            (401, "{}".into()),
            (200, r#"{"access_token":"NEWER","expires_in":3600}"#.into()),
            (200, r#"{"data":[],"paging":{}}"#.into()),
        ]);
        let ep = endpoints(&server.base);
        let engine = Engine {
            db: &db,
            store: &store,
            ep: &ep,
        };
        engine.sync(Service::Mal).unwrap();
        let seen = server.finish();
        assert_eq!(
            seen[0].body,
            "client_id=CID&grant_type=refresh_token&refresh_token=REFRESH"
        );
        assert_eq!(seen[1].header("authorization"), Some("Bearer NEW"));
        assert_eq!(
            seen[2].body,
            "client_id=CID&grant_type=refresh_token&refresh_token=R2"
        );
        assert_eq!(seen[3].header("authorization"), Some("Bearer NEWER"));
        let stored = store.get(Service::Mal).unwrap().unwrap();
        assert_eq!(stored.access, "NEWER");
        assert_eq!(stored.refresh.as_deref(), Some("R2"));

        // A refresh token MAL rejects disconnects.
        store.tokens.lock().unwrap().insert(
            Service::Mal,
            Tokens {
                expires_at: Some(0),
                ..token("OLD")
            },
        );
        let server = Server::start(vec![(400, r#"{"error":"invalid_grant"}"#.into())]);
        let ep = endpoints(&server.base);
        let engine = Engine {
            db: &db,
            store: &store,
            ep: &ep,
        };
        assert!(is_auth_lost(&engine.sync(Service::Mal).unwrap_err()));
        server.finish();
        assert!(
            !db.lock()
                .unwrap()
                .list_account(Service::Mal)
                .unwrap()
                .connected()
        );
    }

    #[test]
    fn connect_anilist_checks_the_token_and_needs_a_keychain() {
        let jwt = "eyJhbGciOi.eyJzdWIiOiI0MiJ9.c2lnbmF0dXJl";
        let db = Mutex::new(Db::open_in_memory().unwrap());
        let broken = MemoryStore {
            broken: true,
            ..Default::default()
        };
        let server = Server::start(vec![
            (200, r#"{"data":{"Viewer":{"id":42,"name":"kev"}}}"#.into()),
            (200, r#"{"data":{"Viewer":{"id":42,"name":"kev"}}}"#.into()),
            (400, r#"{"errors":[{"message":"Invalid token"}]}"#.into()),
        ]);
        let ep = endpoints(&server.base);
        // No keychain: refused, nothing connected, nothing written anywhere.
        let engine = Engine {
            db: &db,
            store: &broken,
            ep: &ep,
        };
        let err = engine.connect_anilist("123", jwt).unwrap_err();
        assert!(err.to_string().contains("keychain"), "{err:#}");
        assert!(
            !db.lock()
                .unwrap()
                .list_account(Service::AniList)
                .unwrap()
                .connected()
        );

        let store = MemoryStore::default();
        let engine = Engine {
            db: &db,
            store: &store,
            ep: &ep,
        };
        assert!(engine.connect_anilist("123", "not a token").is_err());
        assert_eq!(
            engine
                .connect_anilist("123", &format!("  {jwt}\n"))
                .unwrap(),
            "kev"
        );
        assert_eq!(store.get(Service::AniList).unwrap().unwrap().access, jwt);
        let account = db.lock().unwrap().list_account(Service::AniList).unwrap();
        assert!(account.connected());
        assert_eq!(account.client_id, "123");
        let err = engine.connect_anilist("123", jwt).unwrap_err();
        assert!(err.to_string().contains("didn't accept"), "{err:#}");
        let seen = server.finish();
        assert_eq!(
            seen[1].header("authorization"),
            Some(&*format!("Bearer {jwt}"))
        );
    }

    #[test]
    fn finish_mal_stores_tokens_and_disconnect_forgets() {
        let db = Mutex::new(Db::open_in_memory().unwrap());
        let store = MemoryStore::default();
        let server = Server::start(vec![
            (
                200,
                r#"{"access_token":"A","refresh_token":"R","expires_in":3600}"#.into(),
            ),
            (200, r#"{"id":7,"name":"kev"}"#.into()),
        ]);
        let ep = endpoints(&server.base);
        let engine = Engine {
            db: &db,
            store: &store,
            ep: &ep,
        };
        assert_eq!(engine.finish_mal("CID", "CODE", "VERIFIER").unwrap(), "kev");
        let seen = server.finish();
        assert!(seen[0].body.contains("code_verifier=VERIFIER"));
        assert_eq!(seen[1].header("authorization"), Some("Bearer A"));
        assert_eq!(
            store.get(Service::Mal).unwrap().unwrap().refresh.as_deref(),
            Some("R")
        );
        assert!(
            db.lock()
                .unwrap()
                .list_account(Service::Mal)
                .unwrap()
                .connected()
        );

        engine.disconnect(Service::Mal).unwrap();
        assert!(store.get(Service::Mal).unwrap().is_none());
        let account = db.lock().unwrap().list_account(Service::Mal).unwrap();
        assert!(!account.connected() && account.user_name.is_none());
        assert_eq!(account.client_id, "CID");
    }

    #[test]
    fn begin_mal_validates_before_the_browser() {
        let db = Mutex::new(Db::open_in_memory().unwrap());
        let store = MemoryStore::default();
        let ep = Endpoints::default();
        let engine = Engine {
            db: &db,
            store: &store,
            ep: &ep,
        };
        for bad in ["", "  ", "abc&redirect_uri=x", "id with spaces"] {
            assert!(engine.begin_mal(bad).is_err(), "{bad:?}");
        }
        let broken = MemoryStore {
            broken: true,
            ..Default::default()
        };
        let engine = Engine {
            db: &db,
            store: &broken,
            ep: &ep,
        };
        assert!(engine.begin_mal("abc123").is_err());
        assert_eq!(
            db.lock()
                .unwrap()
                .list_account(Service::Mal)
                .unwrap()
                .client_id,
            ""
        );
    }

    #[test]
    fn rating_needs_a_connected_account() {
        let db = Mutex::new(Db::open_in_memory().unwrap());
        let store = MemoryStore::default();
        let ep = Endpoints::default();
        let engine = Engine {
            db: &db,
            store: &store,
            ep: &ep,
        };
        assert!(engine.rate(1, 8.0).is_err());
        assert!(
            engine
                .episode_completed("x", "1")
                .unwrap()
                .pushed
                .is_empty()
        );
    }
}
