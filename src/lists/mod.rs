//! MyAnimeList and AniList accounts: importing what the user watched,
//! pushing finished episodes and ratings back, and the merged view the UI
//! shows (My List, the Show page, Continue Watching).
//!
//! AniList media ids are the common key (`shows.anilist_id`); MAL ids are
//! mapped to them through AniList and cached. Tokens live only in the OS
//! keychain (`secrets`). Everything here blocks on the network, so call it
//! off the UI thread; `progress_saved` spawns its own thread.

mod anilist_sync;
mod http;
mod mal;
mod oauth;
mod secrets;
mod sync;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use anyhow::{Result, anyhow};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};

use crate::db::{self, Db, lock};
use crate::model::Progress;
use crate::progress;

pub use oauth::REDIRECT_URI as MAL_REDIRECT_URI;
pub use sync::Endpoints;

/// AniList's PIN page: the redirect URL to register for the implicit grant.
pub const ANILIST_REDIRECT_URI: &str = "https://anilist.co/api/v2/oauth/pin";
pub const MAL_DEVELOPER_PAGE: &str = "https://myanimelist.net/apiconfig";
pub const ANILIST_DEVELOPER_PAGE: &str = "https://anilist.co/settings/developer";

/// Client ID baked into official release builds, if any. Local and source
/// builds leave the env var unset, so Settings asks for one.
pub fn builtin_client_id(service: Service) -> Option<&'static str> {
    match service {
        Service::Mal => option_env!("ANI_GUI_MAL_CLIENT_ID")
            .map(str::trim)
            .filter(|s| !s.is_empty()),
        Service::AniList => None,
    }
}

/// Resolves which client ID to use: a non-empty user override wins over the
/// build's default.
pub fn effective_client_id<'a>(custom: &'a str, builtin: Option<&'a str>) -> &'a str {
    let custom = custom.trim();
    if !custom.is_empty() {
        custom
    } else {
        builtin.map(str::trim).unwrap_or("")
    }
}

/// How long after a sync the app-start sync skips an account.
const STARTUP_SYNC_AFTER_SECS: i64 = 30 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Service {
    Mal,
    AniList,
}

impl Service {
    pub const ALL: [Service; 2] = [Service::Mal, Service::AniList];

    pub fn as_str(self) -> &'static str {
        match self {
            Service::Mal => "mal",
            Service::AniList => "anilist",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Service::Mal => "MyAnimeList",
            Service::AniList => "AniList",
        }
    }
}

impl FromStr for Service {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "mal" => Ok(Service::Mal),
            "anilist" => Ok(Service::AniList),
            other => Err(anyhow!("unknown list service: {other:?}")),
        }
    }
}

impl fmt::Display for Service {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// A list status both services share. AniList's "rewatching" counts as
/// watching.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ListStatus {
    Watching,
    Completed,
    Planning,
    Paused,
    Dropped,
}

impl ListStatus {
    /// My List's group order.
    pub const ALL: [ListStatus; 5] = [
        ListStatus::Watching,
        ListStatus::Completed,
        ListStatus::Planning,
        ListStatus::Paused,
        ListStatus::Dropped,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ListStatus::Watching => "watching",
            ListStatus::Completed => "completed",
            ListStatus::Planning => "planning",
            ListStatus::Paused => "paused",
            ListStatus::Dropped => "dropped",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ListStatus::Watching => "Watching",
            ListStatus::Completed => "Completed",
            ListStatus::Planning => "Plan to watch",
            ListStatus::Paused => "On hold",
            ListStatus::Dropped => "Dropped",
        }
    }

    pub fn from_mal(s: &str) -> Option<ListStatus> {
        match s {
            "watching" => Some(ListStatus::Watching),
            "completed" => Some(ListStatus::Completed),
            "plan_to_watch" => Some(ListStatus::Planning),
            "on_hold" => Some(ListStatus::Paused),
            "dropped" => Some(ListStatus::Dropped),
            _ => None,
        }
    }

    pub fn to_mal(self) -> &'static str {
        match self {
            ListStatus::Watching => "watching",
            ListStatus::Completed => "completed",
            ListStatus::Planning => "plan_to_watch",
            ListStatus::Paused => "on_hold",
            ListStatus::Dropped => "dropped",
        }
    }

    pub fn from_anilist(s: &str) -> Option<ListStatus> {
        match s {
            "CURRENT" | "REPEATING" => Some(ListStatus::Watching),
            "COMPLETED" => Some(ListStatus::Completed),
            "PLANNING" => Some(ListStatus::Planning),
            "PAUSED" => Some(ListStatus::Paused),
            "DROPPED" => Some(ListStatus::Dropped),
            _ => None,
        }
    }

    pub fn to_anilist(self) -> &'static str {
        match self {
            ListStatus::Watching => "CURRENT",
            ListStatus::Completed => "COMPLETED",
            ListStatus::Planning => "PLANNING",
            ListStatus::Paused => "PAUSED",
            ListStatus::Dropped => "DROPPED",
        }
    }
}

impl FromStr for ListStatus {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        ListStatus::ALL
            .into_iter()
            .find(|status| status.as_str() == s)
            .ok_or_else(|| anyhow!("unknown list status: {s:?}"))
    }
}

/// One connected (or once connected) account. A row exists as soon as a
/// client ID is saved.
#[derive(Clone, Debug, PartialEq)]
pub struct Account {
    pub service: Service,
    pub client_id: String,
    pub user_name: Option<String>,
    /// Set while connected.
    pub connected_at: Option<i64>,
    pub last_sync: Option<i64>,
    pub last_error: Option<String>,
}

impl Account {
    pub fn empty(service: Service) -> Account {
        Account {
            service,
            client_id: String::new(),
            user_name: None,
            connected_at: None,
            last_sync: None,
            last_error: None,
        }
    }

    pub fn connected(&self) -> bool {
        self.connected_at.is_some()
    }
}

/// One anime on one service's list, as last imported or pushed.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteEntry {
    pub service: Service,
    /// The service's own media id (MAL id or AniList id).
    pub remote_id: i64,
    /// None for a MAL entry AniList has no match for (yet).
    pub anilist_id: Option<i64>,
    pub mal_id: Option<i64>,
    pub status: ListStatus,
    pub progress: u32,
    /// 1..=10; None when unrated.
    pub score: Option<f32>,
    /// Episode count, when the service knows it.
    pub episodes: Option<u32>,
    pub title: String,
    pub cover_url: String,
    /// Unix seconds of the service's last change.
    pub updated_at: i64,
}

/// One anime across both services (see `merge`).
#[derive(Clone, Debug, PartialEq)]
pub struct ListEntry {
    pub anilist_id: i64,
    pub mal_id: Option<i64>,
    pub title: String,
    pub cover_url: String,
    pub status: ListStatus,
    pub progress: u32,
    pub score: Option<f32>,
    pub episodes: Option<u32>,
    pub updated_at: i64,
}

/// A change waiting to reach one service: the whole desired entry, so a
/// retry needs nothing else.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingPush {
    pub service: Service,
    pub anilist_id: i64,
    pub status: ListStatus,
    pub progress: u32,
    /// None leaves the service's score alone.
    pub score: Option<f32>,
}

/// Combines both services' entries per AniList id. The most recently
/// updated entry wins status and progress (ties go to the higher progress);
/// its score wins too unless it has none and the other has one. AniList's
/// title and cover are preferred, since its titles match the rest of the
/// app. Entries without an AniList id are left out. Newest first.
pub fn merge(entries: &[RemoteEntry]) -> Vec<ListEntry> {
    let mut by_id: HashMap<i64, Vec<&RemoteEntry>> = HashMap::new();
    for entry in entries {
        if let Some(id) = entry.anilist_id {
            by_id.entry(id).or_default().push(entry);
        }
    }
    let mut merged: Vec<ListEntry> = by_id
        .into_iter()
        .filter_map(|(anilist_id, group)| {
            let newest = group
                .iter()
                .max_by_key(|e| (e.updated_at, e.progress, e.service == Service::AniList))?;
            let named = group
                .iter()
                .find(|e| e.service == Service::AniList && !e.title.is_empty())
                .unwrap_or(newest);
            let first = |f: fn(&RemoteEntry) -> Option<i64>| group.iter().find_map(|e| f(e));
            Some(ListEntry {
                anilist_id,
                mal_id: first(|e| e.mal_id),
                title: named.title.clone(),
                cover_url: if named.cover_url.is_empty() {
                    group
                        .iter()
                        .map(|e| e.cover_url.clone())
                        .find(|c| !c.is_empty())
                        .unwrap_or_default()
                } else {
                    named.cover_url.clone()
                },
                status: newest.status,
                progress: newest.progress,
                score: newest.score.or_else(|| group.iter().find_map(|e| e.score)),
                episodes: newest
                    .episodes
                    .or_else(|| group.iter().find_map(|e| e.episodes)),
                updated_at: newest.updated_at,
            })
        })
        .collect();
    merged.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then(a.anilist_id.cmp(&b.anilist_id))
    });
    merged
}

/// The whole episode number in an episode label ("5", "05"), if it is one;
/// recaps like "5.5" don't count towards list progress.
pub fn whole_episode(ep: &str) -> Option<u32> {
    let n: f64 = ep.trim().parse().ok()?;
    (n.is_finite() && n >= 1.0 && n.fract() == 0.0 && n <= f64::from(u32::MAX)).then_some(n as u32)
}

/// What to push after finishing episode `ep` of a show with `total`
/// episodes (if known), given a service's `current` (status, progress):
/// progress moves up to `ep` (never past `total`) and the status becomes
/// watching, or completed with the final episode. None when the service
/// already has that progress.
pub fn progress_push(
    current: Option<(ListStatus, u32)>,
    ep: u32,
    total: Option<u32>,
) -> Option<(ListStatus, u32)> {
    let target = total.filter(|&t| t > 0).map_or(ep, |t| ep.min(t));
    let have = current.map_or(0, |(_, progress)| progress);
    if target == 0 || target <= have {
        return None;
    }
    let done = total.is_some_and(|t| t > 0 && target >= t);
    Some((
        if done {
            ListStatus::Completed
        } else {
            ListStatus::Watching
        },
        target,
    ))
}

/// A score picked in the UI as stored: 1..=10, or None for anything else.
pub fn valid_score(score: f32) -> Option<f32> {
    (score.is_finite() && (1.0..=10.0).contains(&score)).then_some(score)
}

/// Every account, connected or not, in `Service::ALL` order.
pub fn accounts(db: &Mutex<Db>) -> Vec<Account> {
    let rows = lock(db).list_accounts().unwrap_or_else(|err| {
        eprintln!("ani-gui: reading accounts: {err:#}");
        Vec::new()
    });
    Service::ALL
        .into_iter()
        .map(|s| {
            rows.iter()
                .find(|a| a.service == s)
                .cloned()
                .unwrap_or_else(|| Account::empty(s))
        })
        .collect()
}

/// The merged list (newest first).
pub fn my_list(db: &Mutex<Db>) -> Result<Vec<ListEntry>> {
    Ok(merge(&lock(db).list_entries()?))
}

/// The merged entry for one show, if any service lists it.
pub fn entry_for(db: &Mutex<Db>, anilist_id: i64) -> Option<ListEntry> {
    let entries = lock(db)
        .list_entries_for(anilist_id)
        .inspect_err(|err| eprintln!("ani-gui: reading list entry {anilist_id}: {err:#}"))
        .ok()?;
    merge(&entries).into_iter().next()
}

/// Shows the user is watching on a service, newest first.
pub fn watching(db: &Mutex<Db>) -> Vec<ListEntry> {
    my_list(db)
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.status == ListStatus::Watching)
        .collect()
}

/// Whether the user should be asked to rate this finished show.
pub fn wants_rating(db: &Mutex<Db>, anilist_id: i64) -> bool {
    lock(db).rating_prompt(anilist_id).unwrap_or(false)
}

/// What the UI hears about from the background.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Accounts or list data changed.
    Changed,
    /// The final episode of this show was just marked on a list.
    Finished { title: String },
    /// A service rejected its token; the account is now disconnected.
    AuthLost(Service),
}

struct Lists {
    db: Arc<Mutex<Db>>,
    store: secrets::Keyring,
    endpoints: Endpoints,
    events: UnboundedSender<Event>,
    /// One network job (sync, push, connect) at a time.
    busy: Mutex<()>,
    syncing: Mutex<HashSet<Service>>,
    /// (show id, episode) completions already handled this session: the
    /// player saves the same finished episode many times.
    handled: Mutex<HashSet<(String, String)>>,
}

static LISTS: OnceLock<Lists> = OnceLock::new();

impl Lists {
    fn engine(&self) -> sync::Engine<'_> {
        sync::Engine {
            db: &self.db,
            store: &self.store,
            ep: &self.endpoints,
        }
    }

    fn emit(&self, event: Event) {
        let _ = self.events.unbounded_send(event);
    }

    fn busy(&self) -> std::sync::MutexGuard<'_, ()> {
        self.busy.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn report(&self, service: Service, result: &Result<()>) {
        if let Err(err) = result
            && sync::is_auth_lost(err)
        {
            self.emit(Event::AuthLost(service));
        }
        self.emit(Event::Changed);
    }
}

/// Starts list syncing for the app's database. Returns the event stream
/// the first time; None if already started.
pub fn init(db: Arc<Mutex<Db>>) -> Option<UnboundedReceiver<Event>> {
    let (tx, rx) = unbounded();
    let lists = Lists {
        db,
        store: secrets::Keyring,
        endpoints: Endpoints::default(),
        events: tx,
        busy: Mutex::new(()),
        syncing: Mutex::new(HashSet::new()),
        handled: Mutex::new(HashSet::new()),
    };
    LISTS.set(lists).ok().map(|()| rx)
}

fn lists() -> Result<&'static Lists> {
    LISTS
        .get()
        .ok_or_else(|| anyhow!("account syncing hasn't started"))
}

/// The player's hook: called with every saved progress. A finished episode
/// of a show with an AniList id is pushed to each connected service on a
/// thread of its own.
pub fn progress_saved(p: &Progress) {
    let Some(lists) = LISTS.get() else {
        return;
    };
    if !progress::is_completed(p) || whole_episode(&p.ep).is_none() {
        return;
    }
    let key = (p.show_id.clone(), p.ep.clone());
    if !lists
        .handled
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key)
    {
        return;
    }
    let (show_id, ep) = (p.show_id.clone(), p.ep.clone());
    let spawned = std::thread::Builder::new()
        .name("list-push".into())
        .spawn(move || {
            let finished = {
                let _busy = lists.busy();
                lists.engine().episode_completed(&show_id, &ep)
            };
            match finished {
                Ok(outcome) => {
                    for (service, result) in &outcome.pushed {
                        if let Err(err) = result {
                            eprintln!("ani-gui: updating {service}: {err:#}");
                        }
                        lists.report(*service, result);
                    }
                    if let Some(title) = outcome.finished {
                        lists.emit(Event::Finished { title });
                    }
                }
                Err(err) => eprintln!("ani-gui: updating lists: {err:#}"),
            }
        });
    if let Err(err) = spawned {
        eprintln!("ani-gui: starting list update: {err}");
    }
}

/// Syncs every connected account in the background; without `force`, only
/// those not synced in the last half hour (app start).
pub fn sync_all_background(force: bool) {
    let Ok(lists) = lists() else {
        return;
    };
    let now = db::now();
    let due: Vec<Service> = accounts(&lists.db)
        .into_iter()
        .filter(|a| a.connected())
        .filter(|a| {
            force
                || a.last_sync
                    .is_none_or(|t| now - t >= STARTUP_SYNC_AFTER_SECS)
        })
        .map(|a| a.service)
        .collect();
    if due.is_empty() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("list-sync".into())
        .spawn(move || {
            for service in due {
                if let Err(err) = sync_now(service) {
                    eprintln!("ani-gui: syncing {service}: {err:#}");
                }
            }
        });
    if let Err(err) = spawned {
        eprintln!("ani-gui: starting list sync: {err}");
    }
}

/// Retries pending pushes, then imports the whole list. Blocks.
pub fn sync_now(service: Service) -> Result<()> {
    let lists = lists()?;
    lists
        .syncing
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(service);
    lists.emit(Event::Changed);
    let result = {
        let _busy = lists.busy();
        lists.engine().sync(service)
    };
    lists
        .syncing
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&service);
    lists.report(service, &result);
    result
}

pub fn is_syncing(service: Service) -> bool {
    LISTS.get().is_some_and(|l| {
        l.syncing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&service)
    })
}

/// Saves the client ID typed in Settings.
pub fn set_client_id(service: Service, client_id: &str) -> Result<()> {
    let lists = lists()?;
    lock(&lists.db).set_list_client_id(service, client_id.trim())?;
    lists.emit(Event::Changed);
    Ok(())
}

/// The MyAnimeList sign-in: opens the browser, waits (up to a few minutes,
/// or until `cancel`) for its redirect, then imports the list. Blocks;
/// returns the user name.
pub fn connect_mal(client_id: &str, cancel: Arc<AtomicBool>) -> Result<String> {
    let lists = lists()?;
    let client_id = effective_client_id(client_id, builtin_client_id(Service::Mal));
    let name = lists
        .engine()
        .connect_mal(client_id, &cancel, || lists.busy());
    lists.emit(Event::Changed);
    let name = name?;
    let _ = sync_now(Service::Mal);
    Ok(name)
}

/// The URL that shows the AniList token to paste.
pub fn anilist_authorize_url(client_id: &str) -> Result<String> {
    anilist_sync::authorize_url(client_id)
}

/// Checks a pasted AniList token, stores it, then imports the list.
/// Blocks; returns the user name.
pub fn connect_anilist(client_id: &str, token: &str) -> Result<String> {
    let lists = lists()?;
    let name = {
        let _busy = lists.busy();
        lists.engine().connect_anilist(client_id, token)
    };
    lists.emit(Event::Changed);
    let name = name?;
    let _ = sync_now(Service::AniList);
    Ok(name)
}

/// Forgets the account: its token, imported entries and queued pushes.
pub fn disconnect(service: Service) -> Result<()> {
    let lists = lists()?;
    let result = {
        let _busy = lists.busy();
        lists.engine().disconnect(service)
    };
    lists.emit(Event::Changed);
    result
}

/// Rates a show on every connected service. Blocks.
pub fn rate(anilist_id: i64, score: f32) -> Result<()> {
    let lists = lists()?;
    let score = valid_score(score).ok_or_else(|| anyhow!("scores go from 1 to 10"))?;
    let results = {
        let _busy = lists.busy();
        lists.engine().rate(anilist_id, score)
    };
    let mut first_err = None;
    for (service, result) in results? {
        lists.report(service, &result);
        if let Err(err) = result {
            first_err.get_or_insert(anyhow!("{service}: {err:#}"));
        }
    }
    lists.emit(Event::Changed);
    match first_err {
        // The score is queued and retried on the next sync.
        Some(err) => Err(err.context("saved here, will retry")),
        None => Ok(()),
    }
}

/// "Not now" on the rating prompt.
pub fn dismiss_rating(anilist_id: i64) -> Result<()> {
    let lists = lists()?;
    lock(&lists.db).clear_rating_prompt(anilist_id)?;
    lists.emit(Event::Changed);
    Ok(())
}

/// Opens `url` (https only) in the default browser.
pub fn open_in_browser(url: &str) -> Result<()> {
    oauth::open_browser(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn entry(service: Service, anilist_id: i64, updated_at: i64) -> RemoteEntry {
        RemoteEntry {
            service,
            remote_id: match service {
                Service::AniList => anilist_id,
                Service::Mal => anilist_id + 100_000,
            },
            anilist_id: Some(anilist_id),
            mal_id: Some(anilist_id + 100_000),
            status: ListStatus::Watching,
            progress: 3,
            score: None,
            episodes: Some(12),
            title: format!("{} {anilist_id}", service.as_str()),
            cover_url: format!("https://img/{}/{anilist_id}.jpg", service.as_str()),
            updated_at,
        }
    }

    #[test]
    fn status_strings_roundtrip() {
        for status in ListStatus::ALL {
            assert_eq!(status.as_str().parse::<ListStatus>().ok(), Some(status));
            assert_eq!(ListStatus::from_mal(status.to_mal()), Some(status));
            assert_eq!(ListStatus::from_anilist(status.to_anilist()), Some(status));
        }
        assert_eq!(
            ListStatus::from_anilist("REPEATING"),
            Some(ListStatus::Watching)
        );
        assert_eq!(ListStatus::from_mal("CURRENT"), None);
        assert_eq!(ListStatus::from_anilist("watching"), None);
        for service in Service::ALL {
            assert_eq!(service.as_str().parse::<Service>().ok(), Some(service));
        }
        assert!("MAL".parse::<Service>().is_err());
    }

    #[test]
    fn merge_newest_wins() {
        let mut mal = entry(Service::Mal, 1, 200);
        mal.status = ListStatus::Completed;
        mal.progress = 12;
        let anilist = entry(Service::AniList, 1, 100);
        let merged = merge(&[anilist.clone(), mal.clone()]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].status, ListStatus::Completed);
        assert_eq!(merged[0].progress, 12);
        assert_eq!(merged[0].updated_at, 200);
        // AniList's title and cover, whichever entry is newer.
        assert_eq!(merged[0].title, "anilist 1");
        assert_eq!(merged[0].cover_url, "https://img/anilist/1.jpg");
        assert_eq!(merged[0].mal_id, Some(100_001));

        // The other way round.
        let newer_anilist = RemoteEntry {
            updated_at: 300,
            progress: 5,
            ..anilist
        };
        let merged = merge(&[mal, newer_anilist]);
        assert_eq!(merged[0].status, ListStatus::Watching);
        assert_eq!(merged[0].progress, 5);
    }

    #[test]
    fn merge_keeps_an_older_score_when_the_newest_has_none() {
        let mut mal = entry(Service::Mal, 1, 100);
        mal.score = Some(8.0);
        let anilist = entry(Service::AniList, 1, 200);
        assert_eq!(merge(&[mal.clone(), anilist.clone()])[0].score, Some(8.0));
        let rated = RemoteEntry {
            score: Some(9.5),
            ..anilist
        };
        assert_eq!(merge(&[mal, rated])[0].score, Some(9.5));
    }

    #[test]
    fn merge_ties_go_to_more_progress_and_skips_unmapped() {
        let a = entry(Service::Mal, 1, 100);
        let b = RemoteEntry {
            progress: 7,
            ..entry(Service::AniList, 1, 100)
        };
        assert_eq!(merge(&[a.clone(), b.clone()])[0].progress, 7);
        assert_eq!(merge(&[b, a])[0].progress, 7);

        let unmapped = RemoteEntry {
            anilist_id: None,
            ..entry(Service::Mal, 2, 500)
        };
        let only_mal = RemoteEntry {
            title: "MAL title".into(),
            ..entry(Service::Mal, 3, 50)
        };
        let merged = merge(&[unmapped, only_mal, entry(Service::AniList, 4, 60)]);
        let ids: Vec<i64> = merged.iter().map(|e| e.anilist_id).collect();
        // Newest first; the MAL-only entry keeps MAL's title.
        assert_eq!(ids, [4, 3]);
        assert_eq!(merged[1].title, "MAL title");
    }

    #[test]
    fn whole_episode_only_counts_plain_numbers() {
        assert_eq!(whole_episode("5"), Some(5));
        assert_eq!(whole_episode("05"), Some(5));
        assert_eq!(whole_episode("12.0"), Some(12));
        for bad in ["5.5", "0", "-1", "", "x", "inf", "NaN", "1e99"] {
            assert_eq!(whole_episode(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn progress_push_moves_forward_only() {
        use ListStatus::*;
        // Not on the list yet.
        assert_eq!(progress_push(None, 1, Some(12)), Some((Watching, 1)));
        assert_eq!(
            progress_push(Some((Planning, 0)), 3, Some(12)),
            Some((Watching, 3))
        );
        // Already there, or further.
        assert_eq!(progress_push(Some((Watching, 3)), 3, Some(12)), None);
        assert_eq!(progress_push(Some((Watching, 8)), 3, Some(12)), None);
        // A rewatch of a finished show changes nothing.
        assert_eq!(progress_push(Some((Completed, 12)), 1, Some(12)), None);
        // The final episode completes it.
        assert_eq!(
            progress_push(Some((Watching, 11)), 12, Some(12)),
            Some((Completed, 12))
        );
        // Unknown length (still airing) stays watching.
        assert_eq!(
            progress_push(Some((Watching, 11)), 12, None),
            Some((Watching, 12))
        );
        assert_eq!(progress_push(None, 5, Some(0)), Some((Watching, 5)));
        // Numbering past the episode count is clamped to it.
        assert_eq!(
            progress_push(Some((Watching, 3)), 13, Some(12)),
            Some((Completed, 12))
        );
        assert_eq!(progress_push(Some((Completed, 12)), 13, Some(12)), None);
        assert_eq!(progress_push(None, 0, Some(12)), None);
    }

    #[test]
    fn valid_score_range() {
        assert_eq!(valid_score(1.0), Some(1.0));
        assert_eq!(valid_score(10.0), Some(10.0));
        assert_eq!(valid_score(7.5), Some(7.5));
        for bad in [0.0, 0.5, 10.5, -3.0, f32::NAN, f32::INFINITY] {
            assert_eq!(valid_score(bad), None, "{bad}");
        }
    }

    #[test]
    fn effective_client_id_prefers_custom_over_builtin() {
        assert_eq!(effective_client_id("custom", Some("builtin")), "custom");
        assert_eq!(effective_client_id("  custom  ", Some("builtin")), "custom");
        assert_eq!(effective_client_id("", Some("builtin")), "builtin");
        assert_eq!(effective_client_id("   ", Some(" builtin ")), "builtin");
        assert_eq!(effective_client_id("", None), "");
        assert_eq!(effective_client_id("custom", None), "custom");
    }
}
