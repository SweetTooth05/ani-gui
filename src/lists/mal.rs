//! MyAnimeList's API v2: OAuth token exchange and refresh, the user's
//! anime list, and list updates. `oauth` and `api` are base URLs
//! (`Endpoints`), so tests can point them at a local server.

use serde_json::Value;

use super::http::{self, HttpError};
use super::oauth::{self, REDIRECT_URI};
use super::secrets::Tokens;
use super::{ListStatus, RemoteEntry, Service};

pub const OAUTH: &str = "https://myanimelist.net/v1/oauth2";
pub const API: &str = "https://api.myanimelist.net/v2";
/// MAL's page size limit for list reads.
const PAGE: u32 = 1000;
/// Enough for 50,000 entries; stops a server that keeps paging forever.
const MAX_PAGES: usize = 50;

/// Where to send the user to approve access. MAL only does PKCE `plain`,
/// so the challenge is the verifier itself.
pub fn authorize_url(oauth: &str, client_id: &str, verifier: &str, state: &str) -> String {
    format!(
        "{oauth}/authorize?response_type=code&client_id={}&state={}&code_challenge={}\
         &code_challenge_method=plain&redirect_uri={}",
        oauth::encode(client_id),
        oauth::encode(state),
        oauth::encode(verifier),
        oauth::encode(REDIRECT_URI),
    )
}

pub fn exchange_code(
    oauth: &str,
    client_id: &str,
    code: &str,
    verifier: &str,
    now: i64,
) -> Result<Tokens, HttpError> {
    let v = http::post_form(
        &format!("{oauth}/token"),
        None,
        &[
            ("client_id", client_id),
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", REDIRECT_URI),
            ("code_verifier", verifier),
        ],
    )?;
    parse_tokens(&v, None, now)
}

/// A new access token from the refresh token. MAL may or may not rotate
/// the refresh token; the old one is kept when it doesn't.
pub fn refresh(
    oauth: &str,
    client_id: &str,
    refresh_token: &str,
    now: i64,
) -> Result<Tokens, HttpError> {
    let v = http::post_form(
        &format!("{oauth}/token"),
        None,
        &[
            ("client_id", client_id),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ],
    )?;
    parse_tokens(&v, Some(refresh_token), now)
}

pub fn parse_tokens(v: &Value, old_refresh: Option<&str>, now: i64) -> Result<Tokens, HttpError> {
    let access = v["access_token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| HttpError::Transport("MyAnimeList sent no access token".into()))?;
    let refresh = v["refresh_token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .or(old_refresh)
        .map(str::to_string);
    Ok(Tokens {
        access: access.to_string(),
        refresh,
        expires_at: v["expires_in"].as_i64().map(|secs| now + secs.max(0)),
    })
}

/// The signed-in user's (id, name).
pub fn me(api: &str, token: &str) -> Result<(i64, String), HttpError> {
    let v = http::get(&format!("{api}/users/@me"), Some(token))?;
    match (v["id"].as_i64(), v["name"].as_str()) {
        (Some(id), Some(name)) if !name.is_empty() => Ok((id, name.to_string())),
        _ => Err(HttpError::Transport(
            "MyAnimeList didn't say who you are".into(),
        )),
    }
}

pub fn list_url(api: &str) -> String {
    format!("{api}/users/@me/animelist?fields=list_status,num_episodes&limit={PAGE}&nsfw=true")
}

/// The whole list, following `paging.next` only while it stays on `api`
/// (the token must never go to another host).
pub fn fetch_list(api: &str, token: &str) -> Result<Vec<RemoteEntry>, HttpError> {
    let mut url = list_url(api);
    let mut all = Vec::new();
    for _ in 0..MAX_PAGES {
        let v = http::get(&url, Some(token))?;
        let (entries, next) = parse_list_page(&v);
        all.extend(entries);
        match next {
            Some(next) if same_origin(&next, api) => url = next,
            _ => break,
        }
    }
    Ok(all)
}

/// Whether `url` is on the same scheme, host and port as `base`.
fn same_origin(url: &str, base: &str) -> bool {
    let origin = |u: &str| {
        let (scheme, rest) = u.split_once("://")?;
        let host = rest.split(['/', '?', '#']).next()?;
        Some((scheme.to_ascii_lowercase(), host.to_ascii_lowercase()))
    };
    origin(url).is_some() && origin(url) == origin(base)
}

/// One page of `/users/@me/animelist`, and the next page's URL.
pub fn parse_list_page(v: &Value) -> (Vec<RemoteEntry>, Option<String>) {
    let entries = v["data"]
        .as_array()
        .map(|items| items.iter().filter_map(parse_item).collect())
        .unwrap_or_default();
    let next = v["paging"]["next"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    (entries, next)
}

fn parse_item(item: &Value) -> Option<RemoteEntry> {
    let node = &item["node"];
    let status = &item["list_status"];
    let id = node["id"].as_i64().filter(|&id| id > 0)?;
    let count = |v: &Value| v.as_u64().and_then(|n| u32::try_from(n).ok());
    Some(RemoteEntry {
        service: Service::Mal,
        remote_id: id,
        anilist_id: None,
        mal_id: Some(id),
        status: ListStatus::from_mal(status["status"].as_str()?)?,
        progress: count(&status["num_episodes_watched"]).unwrap_or(0),
        score: status["score"]
            .as_f64()
            .map(|s| s as f32)
            .and_then(super::valid_score),
        episodes: count(&node["num_episodes"]).filter(|&n| n > 0),
        title: node["title"].as_str().unwrap_or_default().to_string(),
        cover_url: node["main_picture"]["large"]
            .as_str()
            .or(node["main_picture"]["medium"].as_str())
            .filter(|u| u.starts_with("https://"))
            .unwrap_or_default()
            .to_string(),
        updated_at: status["updated_at"]
            .as_str()
            .and_then(parse_iso8601)
            .unwrap_or(0),
    })
}

/// The form `PATCH /anime/{id}/my_list_status` takes. MAL scores are whole
/// numbers.
pub fn update_form(
    status: ListStatus,
    progress: u32,
    score: Option<f32>,
) -> Vec<(&'static str, String)> {
    let mut form = vec![
        ("status", status.to_mal().to_string()),
        ("num_watched_episodes", progress.to_string()),
    ];
    if let Some(score) = score {
        form.push(("score", (score.round() as u32).clamp(1, 10).to_string()));
    }
    form
}

pub fn update(
    api: &str,
    token: &str,
    mal_id: i64,
    status: ListStatus,
    progress: u32,
    score: Option<f32>,
) -> Result<(), HttpError> {
    let form = update_form(status, progress, score);
    let pairs: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
    http::patch_form(
        &format!("{api}/anime/{mal_id}/my_list_status"),
        Some(token),
        &pairs,
    )
    .map(|_| ())
}

/// Unix seconds from an RFC 3339 timestamp like MAL's
/// `2024-03-23T10:15:00+00:00` (fractional seconds and `Z` allowed).
pub fn parse_iso8601(s: &str) -> Option<i64> {
    let (date, rest) = s.split_once('T')?;
    let mut ymd = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (y, mo, d) = (ymd.next()??, ymd.next()??, ymd.next()??);
    let tz_at = rest.find(['Z', 'z', '+', '-'])?;
    let (time, tz) = rest.split_at(tz_at);
    let mut hms = time.splitn(3, ':');
    let h: i64 = hms.next()?.parse().ok()?;
    let mi: i64 = hms.next()?.parse().ok()?;
    let sec: i64 = hms.next()?.split('.').next()?.parse().ok()?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let offset = match tz {
        "Z" | "z" => 0,
        _ => {
            let sign = if tz.starts_with('-') { -1 } else { 1 };
            let (oh, om) = tz[1..].split_once(':')?;
            sign * (oh.parse::<i64>().ok()? * 3600 + om.parse::<i64>().ok()? * 60)
        }
    };
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec - offset)
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::http::fake::Server;
    use super::*;

    fn fixture() -> Value {
        let path = format!(
            "{}/tests/fixtures/mal_animelist.json",
            env!("CARGO_MANIFEST_DIR")
        );
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn authorize_url_has_plain_pkce_and_state() {
        let url = authorize_url(OAUTH, "abc123", "VERIFIER-._~", "STATE");
        assert_eq!(
            url,
            "https://myanimelist.net/v1/oauth2/authorize?response_type=code&client_id=abc123\
             &state=STATE&code_challenge=VERIFIER-._~&code_challenge_method=plain\
             &redirect_uri=http%3A%2F%2Flocalhost%3A53682%2Fcallback"
        );
        // A hostile client ID can't add parameters.
        assert!(
            authorize_url(OAUTH, "a&redirect_uri=evil", "v", "s")
                .contains("a%26redirect_uri%3Devil")
        );
    }

    #[test]
    fn parse_list_fixture() {
        let (entries, next) = parse_list_page(&fixture());
        assert_eq!(
            next.as_deref(),
            Some(
                "https://api.myanimelist.net/v2/users/@me/animelist?offset=3&fields=list_status,num_episodes&limit=3"
            )
        );
        assert_eq!(entries.len(), 3);
        let frieren = &entries[0];
        assert_eq!(frieren.remote_id, 52991);
        assert_eq!(frieren.mal_id, Some(52991));
        assert_eq!(frieren.anilist_id, None);
        assert_eq!(frieren.status, ListStatus::Completed);
        assert_eq!(frieren.progress, 28);
        assert_eq!(frieren.score, Some(10.0));
        assert_eq!(frieren.episodes, Some(28));
        assert_eq!(frieren.title, "Sousou no Frieren");
        assert_eq!(
            frieren.cover_url,
            "https://cdn.myanimelist.net/images/anime/1015/138006l.jpg"
        );
        assert_eq!(frieren.updated_at, 1_711_188_900);
        // Unrated (0), unknown length (0), plan to watch.
        assert_eq!(entries[1].status, ListStatus::Watching);
        assert_eq!(entries[1].score, None);
        assert_eq!(entries[1].episodes, None);
        assert_eq!(entries[2].status, ListStatus::Planning);
        assert_eq!(entries[2].cover_url, "");
    }

    #[test]
    fn parse_list_skips_broken_items() {
        let (entries, next) = parse_list_page(&json!({ "data": [
            { "node": { "id": 1 }, "list_status": { "status": "rewatching" } },
            { "node": { "id": -4 }, "list_status": { "status": "watching" } },
            { "node": {}, "list_status": { "status": "watching" } },
            { "node": { "id": 2, "main_picture": { "medium": "javascript:x" } },
              "list_status": { "status": "dropped", "num_episodes_watched": 3, "score": 11 } },
            "junk"
        ], "paging": {} }));
        assert!(next.is_none());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].remote_id, 2);
        assert_eq!(entries[0].score, None);
        assert_eq!(entries[0].cover_url, "");
        assert_eq!(entries[0].updated_at, 0);
        assert!(parse_list_page(&json!(null)).0.is_empty());
    }

    #[test]
    fn iso8601_parsing() {
        assert_eq!(parse_iso8601("1970-01-01T00:00:00+00:00"), Some(0));
        assert_eq!(
            parse_iso8601("2024-03-23T10:15:00+00:00"),
            Some(1_711_188_900)
        );
        assert_eq!(
            parse_iso8601("2024-03-23T12:15:00+02:00"),
            Some(1_711_188_900)
        );
        assert_eq!(
            parse_iso8601("2024-03-23T05:15:00-05:00"),
            Some(1_711_188_900)
        );
        assert_eq!(
            parse_iso8601("2024-03-23T10:15:00.123Z"),
            Some(1_711_188_900)
        );
        assert_eq!(parse_iso8601("2000-02-29T00:00:00Z"), Some(951_782_400));
        for bad in [
            "",
            "2024-03-23",
            "2024-13-01T00:00:00Z",
            "2024-03-23T10:15:00",
            "x-y-zTa:b:cZ",
            "2024-03-23T25:00:00Z",
        ] {
            assert_eq!(parse_iso8601(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn tokens_parse_and_keep_old_refresh() {
        let t = parse_tokens(
            &json!({ "token_type": "Bearer", "expires_in": 2_678_400, "access_token": "A", "refresh_token": "R" }),
            None,
            1000,
        )
        .unwrap();
        assert_eq!(t.access, "A");
        assert_eq!(t.refresh.as_deref(), Some("R"));
        assert_eq!(t.expires_at, Some(2_679_400));
        let t = parse_tokens(
            &json!({ "access_token": "B", "expires_in": 60 }),
            Some("OLD"),
            0,
        )
        .unwrap();
        assert_eq!(t.refresh.as_deref(), Some("OLD"));
        assert!(parse_tokens(&json!({ "access_token": "" }), None, 0).is_err());
    }

    #[test]
    fn update_form_rounds_scores() {
        assert_eq!(
            update_form(ListStatus::Completed, 12, Some(8.6)),
            vec![
                ("status", "completed".to_string()),
                ("num_watched_episodes", "12".to_string()),
                ("score", "9".to_string()),
            ]
        );
        assert_eq!(update_form(ListStatus::Watching, 3, None).len(), 2);
    }

    #[test]
    fn token_exchange_and_refresh_requests() {
        let server = Server::start(vec![
            (
                200,
                r#"{"access_token":"A1","refresh_token":"R1","expires_in":3600}"#.into(),
            ),
            (200, r#"{"access_token":"A2","expires_in":3600}"#.into()),
            (
                400,
                r#"{"error":"invalid_grant","message":"refresh token expired"}"#.into(),
            ),
        ]);
        let t = exchange_code(&server.base, "CID", "CODE", "VER", 10).unwrap();
        assert_eq!((t.access.as_str(), t.expires_at), ("A1", Some(3610)));
        let t = refresh(&server.base, "CID", "R1", 20).unwrap();
        assert_eq!(t.refresh.as_deref(), Some("R1"));
        assert!(matches!(
            refresh(&server.base, "CID", "R1", 30),
            Err(HttpError::Status(400, _))
        ));
        let seen = server.finish();
        assert_eq!(seen[0].method, "POST");
        assert_eq!(seen[0].path, "/token");
        assert_eq!(
            seen[0].body,
            "client_id=CID&grant_type=authorization_code&code=CODE\
             &redirect_uri=http%3A%2F%2Flocalhost%3A53682%2Fcallback&code_verifier=VER"
        );
        assert_eq!(seen[0].header("authorization"), None);
        assert_eq!(
            seen[1].body,
            "client_id=CID&grant_type=refresh_token&refresh_token=R1"
        );
    }

    #[test]
    fn fetch_list_pages_but_never_leaves_the_api_host() {
        let server =
            Server::start_with(|base| {
                vec![
                (200, json!({
                    "data": [{ "node": { "id": 1 }, "list_status": { "status": "watching" } }],
                    "paging": { "next": format!("{base}/users/@me/animelist?offset=1") }
                }).to_string()),
                (200, json!({
                    "data": [{ "node": { "id": 2 }, "list_status": { "status": "completed" } }],
                    "paging": { "next": "https://evil.example/steal" }
                }).to_string()),
            ]
            });
        let entries = fetch_list(&server.base, "TOK").unwrap();
        let ids: Vec<i64> = entries.iter().map(|e| e.remote_id).collect();
        assert_eq!(ids, [1, 2]);
        let seen = server.finish();
        assert_eq!(seen.len(), 2);
        assert_eq!(
            seen[0].path,
            "/users/@me/animelist?fields=list_status,num_episodes&limit=1000&nsfw=true"
        );
        assert_eq!(seen[1].path, "/users/@me/animelist?offset=1");
        assert!(
            seen.iter()
                .all(|r| r.header("authorization") == Some("Bearer TOK"))
        );

        assert!(same_origin("https://api.myanimelist.net/v2/x?y", API));
        assert!(!same_origin("https://evil.example/v2", API));
        assert!(!same_origin("http://api.myanimelist.net/v2", API));
        assert!(!same_origin(
            "https://api.myanimelist.net.evil.example/v2",
            API
        ));
        assert!(!same_origin(
            "https://api.myanimelist.net@evil.example/v2",
            API
        ));
        assert!(!same_origin("garbage", API));
    }

    #[test]
    fn update_and_me_requests() {
        let server = Server::start(vec![
            (200, r#"{"status":"completed"}"#.into()),
            (200, r#"{"id":7,"name":"kev"}"#.into()),
            (401, "{}".into()),
        ]);
        update(
            &server.base,
            "TOK",
            52991,
            ListStatus::Completed,
            28,
            Some(9.0),
        )
        .unwrap();
        assert_eq!(me(&server.base, "TOK").unwrap(), (7, "kev".to_string()));
        assert!(matches!(
            me(&server.base, "TOK"),
            Err(HttpError::Unauthorized)
        ));
        let seen = server.finish();
        assert_eq!(seen[0].method, "PATCH");
        assert_eq!(seen[0].path, "/anime/52991/my_list_status");
        assert_eq!(seen[0].header("authorization"), Some("Bearer TOK"));
        assert_eq!(
            seen[0].body,
            "status=completed&num_watched_episodes=28&score=9"
        );
        assert_eq!(seen[1].path, "/users/@me");
    }
}
