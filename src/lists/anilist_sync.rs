//! AniList's side of list syncing: the viewer, the user's anime list,
//! MAL-to-AniList id mapping, and list updates. Separate from `anilist.rs`,
//! which serves the catalogue's media cards.

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::http::{self, HttpError};
use super::oauth;
use super::{ListStatus, RemoteEntry, Service};

pub const API: &str = "https://graphql.anilist.co";
const AUTHORIZE: &str = "https://anilist.co/api/v2/oauth/authorize";
/// AniList's limit for `idMal_in` pages.
pub const MAP_BATCH: usize = 50;
const PER_CHUNK: u32 = 500;
/// Enough for 10,000 entries; stops a server that keeps chunking forever.
const MAX_CHUNKS: u32 = 20;
/// AniList tokens are JWTs of about a thousand characters.
const MAX_TOKEN: usize = 4096;

const VIEWER_GQL: &str = "query { Viewer { id name } }";

const LIST_GQL: &str = "query ($userId: Int, $chunk: Int, $perChunk: Int) { \
    MediaListCollection(userId: $userId, type: ANIME, chunk: $chunk, perChunk: $perChunk) { \
    hasNextChunk lists { entries { status progress score(format: POINT_10_DECIMAL) updatedAt \
    media { id idMal episodes title { romaji english } coverImage { extraLarge large } } } } } }";

const MAP_GQL: &str = "query ($ids: [Int]) { Page(perPage: 50) { \
    media(idMal_in: $ids, type: ANIME) { id idMal } } }";

const MAL_ID_GQL: &str = "query ($id: Int) { Media(id: $id, type: ANIME) { idMal } }";

const SAVE_GQL: &str = "mutation ($mediaId: Int, $status: MediaListStatus, $progress: Int, \
    $scoreRaw: Int) { SaveMediaListEntry(mediaId: $mediaId, status: $status, \
    progress: $progress, scoreRaw: $scoreRaw) { id } }";

/// The PIN page flow: AniList shows the token for the user to paste.
pub fn authorize_url(client_id: &str) -> Result<String> {
    let id = client_id.trim();
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        bail!("AniList client IDs are numbers. Copy it from anilist.co/settings/developer.");
    }
    Ok(format!(
        "{AUTHORIZE}?client_id={}&response_type=token",
        oauth::encode(id)
    ))
}

/// The pasted token, trimmed, if it looks like one (a JWT: base64url
/// segments separated by dots).
pub fn token_shape(pasted: &str) -> Option<&str> {
    let token = pasted.trim();
    let ok = !token.is_empty()
        && token.len() <= MAX_TOKEN
        && token.split('.').count() == 3
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'='));
    ok.then_some(token)
}

/// Runs a GraphQL query. An "Invalid token" answer counts as 401: AniList
/// sends it as a 400.
fn gql(api: &str, token: Option<&str>, query: &str, variables: Value) -> Result<Value, HttpError> {
    let body = json!({ "query": query, "variables": variables });
    let v = match http::post_json(api, token, &body) {
        Err(HttpError::Status(400, msg)) if msg.eq_ignore_ascii_case("invalid token") => {
            return Err(HttpError::Unauthorized);
        }
        other => other?,
    };
    if v["errors"].as_array().is_some_and(|e| !e.is_empty()) {
        let msg = http::server_message(&v);
        if msg.eq_ignore_ascii_case("invalid token") || msg.eq_ignore_ascii_case("unauthorized") {
            return Err(HttpError::Unauthorized);
        }
        return Err(HttpError::Status(200, msg));
    }
    Ok(v)
}

/// The token owner's (id, name).
pub fn viewer(api: &str, token: &str) -> Result<(i64, String), HttpError> {
    let v = gql(api, Some(token), VIEWER_GQL, json!({}))?;
    let viewer = &v["data"]["Viewer"];
    match (viewer["id"].as_i64(), viewer["name"].as_str()) {
        (Some(id), Some(name)) if !name.is_empty() => Ok((id, name.to_string())),
        _ => Err(HttpError::Transport(
            "AniList didn't say who you are".into(),
        )),
    }
}

/// The user's whole anime list, chunk by chunk.
pub fn fetch_list(api: &str, token: &str, user_id: i64) -> Result<Vec<RemoteEntry>, HttpError> {
    let mut all: Vec<RemoteEntry> = Vec::new();
    for chunk in 1..=MAX_CHUNKS {
        let v = gql(
            api,
            Some(token),
            LIST_GQL,
            json!({ "userId": user_id, "chunk": chunk, "perChunk": PER_CHUNK }),
        )?;
        let (entries, more) = parse_collection(&v);
        for entry in entries {
            if !all.iter().any(|e| e.remote_id == entry.remote_id) {
                all.push(entry);
            }
        }
        if !more {
            break;
        }
    }
    Ok(all)
}

/// One `MediaListCollection` chunk, and whether more follow. An anime in
/// several (custom) lists appears once.
pub fn parse_collection(v: &Value) -> (Vec<RemoteEntry>, bool) {
    let collection = &v["data"]["MediaListCollection"];
    let mut out: Vec<RemoteEntry> = Vec::new();
    let lists = collection["lists"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    for entry in lists
        .iter()
        .filter_map(|l| l["entries"].as_array())
        .flatten()
        .filter_map(parse_entry)
    {
        if !out.iter().any(|e| e.remote_id == entry.remote_id) {
            out.push(entry);
        }
    }
    (out, collection["hasNextChunk"].as_bool() == Some(true))
}

fn parse_entry(e: &Value) -> Option<RemoteEntry> {
    let media = &e["media"];
    let id = media["id"].as_i64().filter(|&id| id > 0)?;
    let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_string);
    let count = |v: &Value| v.as_u64().and_then(|n| u32::try_from(n).ok());
    Some(RemoteEntry {
        service: Service::AniList,
        remote_id: id,
        anilist_id: Some(id),
        mal_id: media["idMal"].as_i64().filter(|&id| id > 0),
        status: ListStatus::from_anilist(e["status"].as_str()?)?,
        progress: count(&e["progress"]).unwrap_or(0),
        score: e["score"]
            .as_f64()
            .map(|s| s as f32)
            .and_then(super::valid_score),
        episodes: count(&media["episodes"]).filter(|&n| n > 0),
        title: text(&media["title"]["english"])
            .or_else(|| text(&media["title"]["romaji"]))
            .unwrap_or_default(),
        cover_url: text(&media["coverImage"]["extraLarge"])
            .or_else(|| text(&media["coverImage"]["large"]))
            .filter(|u| u.starts_with("https://"))
            .unwrap_or_default(),
        updated_at: e["updatedAt"].as_i64().unwrap_or(0),
    })
}

/// (MAL id, AniList id) pairs AniList knows for `mal_ids` (at most
/// `MAP_BATCH`). Public data: no token needed.
pub fn map_mal_ids(api: &str, mal_ids: &[i64]) -> Result<Vec<(i64, i64)>, HttpError> {
    if mal_ids.is_empty() {
        return Ok(Vec::new());
    }
    let ids = &mal_ids[..mal_ids.len().min(MAP_BATCH)];
    let v = gql(api, None, MAP_GQL, json!({ "ids": ids }))?;
    Ok(parse_id_map(&v))
}

pub fn parse_id_map(v: &Value) -> Vec<(i64, i64)> {
    v["data"]["Page"]["media"]
        .as_array()
        .map(|media| {
            media
                .iter()
                .filter_map(|m| Some((m["idMal"].as_i64()?, m["id"].as_i64()?)))
                .filter(|&(mal, anilist)| mal > 0 && anilist > 0)
                .collect()
        })
        .unwrap_or_default()
}

/// The MAL id of an AniList anime, if it has one.
pub fn mal_id_of(api: &str, anilist_id: i64) -> Result<Option<i64>, HttpError> {
    let v = gql(api, None, MAL_ID_GQL, json!({ "id": anilist_id }))?;
    Ok(v["data"]["Media"]["idMal"].as_i64().filter(|&id| id > 0))
}

/// `SaveMediaListEntry`'s variables. `scoreRaw` is the 0–100 scale, which
/// works whatever score format the user picked on AniList.
pub fn save_variables(
    anilist_id: i64,
    status: ListStatus,
    progress: u32,
    score: Option<f32>,
) -> Value {
    let mut vars = json!({
        "mediaId": anilist_id,
        "status": status.to_anilist(),
        "progress": progress,
    });
    if let Some(score) = score {
        vars["scoreRaw"] = json!((score * 10.0).round().clamp(10.0, 100.0) as u32);
    }
    vars
}

pub fn save_entry(
    api: &str,
    token: &str,
    anilist_id: i64,
    status: ListStatus,
    progress: u32,
    score: Option<f32>,
) -> Result<(), HttpError> {
    gql(
        api,
        Some(token),
        SAVE_GQL,
        save_variables(anilist_id, status, progress, score),
    )
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::super::http::fake::Server;
    use super::*;

    fn fixture() -> Value {
        let path = format!(
            "{}/tests/fixtures/anilist_list.json",
            env!("CARGO_MANIFEST_DIR")
        );
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn authorize_url_needs_a_numeric_client_id() {
        assert_eq!(
            authorize_url(" 12345 ").unwrap(),
            "https://anilist.co/api/v2/oauth/authorize?client_id=12345&response_type=token"
        );
        for bad in ["", "abc", "12&response_type=code", "1 2"] {
            assert!(authorize_url(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn token_shape_accepts_jwts_only() {
        assert_eq!(token_shape("  aaa.bbb-_.ccc=\n"), Some("aaa.bbb-_.ccc="));
        for bad in [
            "",
            "abc",
            "a.b",
            "a.b.c.d",
            "a b.c.d",
            "a.b.c\"",
            &"a.b.".repeat(2000),
        ] {
            assert_eq!(token_shape(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn parse_collection_fixture() {
        let (entries, more) = parse_collection(&fixture());
        assert!(!more);
        // Frieren is in two lists but appears once.
        let ids: Vec<i64> = entries.iter().map(|e| e.remote_id).collect();
        assert_eq!(ids, [154587, 171018, 21]);
        let frieren = &entries[0];
        assert_eq!(frieren.anilist_id, Some(154587));
        assert_eq!(frieren.mal_id, Some(52991));
        assert_eq!(frieren.status, ListStatus::Completed);
        assert_eq!(frieren.progress, 28);
        assert_eq!(frieren.score, Some(9.5));
        assert_eq!(frieren.episodes, Some(28));
        assert_eq!(frieren.title, "Frieren: Beyond Journey's End");
        assert_eq!(
            frieren.cover_url,
            "https://s4.anilist.co/file/anilistcdn/media/anime/cover/large/bx154587-n1fmjRv4JQUd.jpg"
        );
        assert_eq!(frieren.updated_at, 1_711_000_000);
        // Rewatching counts as watching; unrated is None; romaji fallback.
        assert_eq!(entries[1].status, ListStatus::Watching);
        assert_eq!(entries[1].score, None);
        assert_eq!(entries[1].title, "Dandadan");
        assert_eq!(entries[1].episodes, None);
        assert_eq!(entries[2].status, ListStatus::Paused);
        assert_eq!(entries[2].mal_id, None);
    }

    #[test]
    fn parse_collection_skips_junk() {
        let v = json!({ "data": { "MediaListCollection": { "hasNextChunk": true, "lists": [
            { "entries": [
                { "status": "CURRENT", "media": null },
                { "status": "WHATEVER", "media": { "id": 1 } },
                { "status": "CURRENT", "progress": -2, "score": 0, "media": { "id": 2,
                  "coverImage": { "large": "http://insecure" } } },
                null
            ] },
            { "entries": null }
        ] } } });
        let (entries, more) = parse_collection(&v);
        assert!(more);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].progress, 0);
        assert_eq!(entries[0].cover_url, "");
        assert!(parse_collection(&json!({})).0.is_empty());
    }

    #[test]
    fn id_map_and_save_variables() {
        let v = json!({ "data": { "Page": { "media": [
            { "id": 154587, "idMal": 52991 }, { "id": 5, "idMal": null }, { "id": 0, "idMal": 3 }
        ] } } });
        assert_eq!(parse_id_map(&v), [(52991, 154587)]);
        assert_eq!(
            save_variables(154587, ListStatus::Completed, 28, Some(8.5)),
            json!({ "mediaId": 154587, "status": "COMPLETED", "progress": 28, "scoreRaw": 85 })
        );
        assert_eq!(
            save_variables(1, ListStatus::Watching, 3, None),
            json!({ "mediaId": 1, "status": "CURRENT", "progress": 3 })
        );
    }

    #[test]
    fn requests_and_invalid_token() {
        let server = Server::start(vec![
            (200, r#"{"data":{"Viewer":{"id":42,"name":"kev"}}}"#.into()),
            (
                400,
                r#"{"errors":[{"message":"Invalid token","status":400}],"data":null}"#.into(),
            ),
            (200, r#"{"data":{"SaveMediaListEntry":{"id":9}}}"#.into()),
            (
                200,
                r#"{"errors":[{"message":"validation"}],"data":null}"#.into(),
            ),
            (
                200,
                r#"{"data":{"Page":{"media":[{"id":154587,"idMal":52991}]}}}"#.into(),
            ),
        ]);
        assert_eq!(viewer(&server.base, "TOK").unwrap(), (42, "kev".into()));
        assert!(matches!(
            viewer(&server.base, "BAD"),
            Err(HttpError::Unauthorized)
        ));
        save_entry(
            &server.base,
            "TOK",
            154587,
            ListStatus::Completed,
            28,
            Some(9.0),
        )
        .unwrap();
        match save_entry(&server.base, "TOK", 1, ListStatus::Watching, 1, None) {
            Err(HttpError::Status(_, msg)) => assert_eq!(msg, "validation"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            map_mal_ids(&server.base, &[52991, 1]).unwrap(),
            [(52991, 154587)]
        );
        let seen = server.finish();
        assert_eq!(seen[0].header("authorization"), Some("Bearer TOK"));
        let body: Value = serde_json::from_str(&seen[2].body).unwrap();
        assert_eq!(body["variables"]["scoreRaw"], 90);
        assert!(
            body["query"]
                .as_str()
                .unwrap()
                .contains("SaveMediaListEntry")
        );
        // Id mapping is public: no token sent.
        assert_eq!(seen[4].header("authorization"), None);
        let body: Value = serde_json::from_str(&seen[4].body).unwrap();
        assert_eq!(body["variables"]["ids"], json!([52991, 1]));
    }
}
