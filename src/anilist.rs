use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::model::{Airing, AniListMedia};
use crate::net;

pub const API: &str = "https://graphql.anilist.co";

const CARD_FRAGMENT: &str = "fragment card on Media { id title { romaji english } \
    coverImage { extraLarge large } bannerImage description genres averageScore \
    seasonYear format status episodes nextAiringEpisode { episode airingAt } }";

const MEDIA_GQL: &str = "query ($id: Int) { Media(id: $id, type: ANIME) { \
    ...card streamingEpisodes { title thumbnail } } }";

const TRENDING_GQL: &str = "query ($n: Int) { Page(perPage: $n) { \
    media(type: ANIME, sort: TRENDING_DESC, isAdult: false) { ...card } } }";

const SCHEDULE_GQL: &str = "query ($page: Int, $from: Int, $to: Int) { \
    Page(page: $page, perPage: 50) { pageInfo { hasNextPage } \
    airingSchedules(airingAt_greater: $from, airingAt_lesser: $to, sort: TIME) { \
    episode airingAt media { ...card } } } }";

const RECOMMENDATIONS_GQL: &str = "query ($ids: [Int]) { Page(perPage: 50) { \
    media(id_in: $ids, type: ANIME) { id recommendations(sort: RATING_DESC, perPage: 10) { \
    nodes { mediaRecommendation { ...card } } } } } }";

const SCHEDULE_MAX_PAGES: i64 = 3;

fn query(gql: &str, variables: Value) -> Result<Value> {
    let body = json!({ "query": format!("{gql} {CARD_FRAGMENT}"), "variables": variables });
    let v = net::post_json(API, &body, &[])?;
    if let Some(err) = v["errors"].as_array().and_then(|e| e.first()) {
        bail!(
            "AniList: {}",
            err["message"].as_str().unwrap_or("unknown error")
        );
    }
    Ok(v)
}

fn text(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

fn num<T: TryFrom<i64>>(v: &Value) -> Option<T> {
    v.as_i64().and_then(|n| T::try_from(n).ok())
}

/// Parses one `Media` object.
pub fn parse_media(v: &Value) -> AniListMedia {
    let title_english = text(&v["title"]["english"]);
    let next = &v["nextAiringEpisode"];
    AniListMedia {
        id: v["id"].as_i64().unwrap_or_default(),
        title_romaji: text(&v["title"]["romaji"])
            .or_else(|| title_english.clone())
            .unwrap_or_default(),
        title_english,
        cover_url: text(&v["coverImage"]["extraLarge"])
            .or_else(|| text(&v["coverImage"]["large"]))
            .unwrap_or_default(),
        banner_url: text(&v["bannerImage"]),
        synopsis: v["description"].as_str().and_then(strip_html),
        genres: v["genres"]
            .as_array()
            .map(|g| g.iter().filter_map(text).collect())
            .unwrap_or_default(),
        score: num(&v["averageScore"]),
        year: num(&v["seasonYear"]),
        format: text(&v["format"]),
        status: text(&v["status"]),
        episodes: num(&v["episodes"]),
        next_airing: num(&next["episode"]).zip(next["airingAt"].as_i64()),
        episode_thumbs: v["streamingEpisodes"]
            .as_array()
            .map(|eps| {
                eps.iter()
                    .filter_map(|e| text(&e["title"]).zip(text(&e["thumbnail"])))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// Plain text from AniList's HTML description: `<br>` becomes a line break,
/// other tags are dropped, runs of blank lines collapse to one.
fn strip_html(html: &str) -> Option<String> {
    let mut plain = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        plain.push_str(&rest[..open]);
        let Some(len) = rest[open..].find('>') else {
            rest = &rest[open..];
            break;
        };
        let name = rest[open + 1..open + len]
            .trim_start_matches('/')
            .split(|c: char| !c.is_ascii_alphanumeric())
            .next()
            .unwrap_or_default();
        if name.eq_ignore_ascii_case("br") {
            plain.push('\n');
        }
        rest = &rest[open + len + 1..];
    }
    plain.push_str(rest);
    let plain = plain
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#039;", "'")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&");

    let mut lines: Vec<&str> = Vec::new();
    for line in plain.lines().map(str::trim) {
        if line.is_empty() && lines.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        lines.push(line);
    }
    if lines.last() == Some(&"") {
        lines.pop();
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// One page of an `airingSchedules` response, plus whether more pages exist.
pub fn parse_schedule(v: &Value) -> (Vec<Airing>, bool) {
    let page = &v["data"]["Page"];
    let items = page["airingSchedules"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|a| a["media"].is_object())
                .filter_map(|a| {
                    Some(Airing {
                        media: parse_media(&a["media"]),
                        episode: num(&a["episode"])?,
                        airing_at: a["airingAt"].as_i64()?,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    (
        items,
        page["pageInfo"]["hasNextPage"].as_bool() == Some(true),
    )
}

/// Recommendations of each seed (in `seeds` order, best rated first),
/// de-duplicated and without the seeds themselves.
pub fn parse_recommendations(v: &Value, seeds: &[i64]) -> Vec<AniListMedia> {
    let mut media: Vec<&Value> = v["data"]["Page"]["media"]
        .as_array()
        .map(|m| m.iter().collect())
        .unwrap_or_default();
    media.sort_by_key(|m| {
        seeds
            .iter()
            .position(|&id| Some(id) == m["id"].as_i64())
            .unwrap_or(usize::MAX)
    });

    let mut seen: HashSet<i64> = seeds.iter().copied().collect();
    media
        .iter()
        .filter_map(|m| m["recommendations"]["nodes"].as_array())
        .flatten()
        .map(|node| &node["mediaRecommendation"])
        .filter(|rec| rec.is_object())
        .map(parse_media)
        .filter(|rec| seen.insert(rec.id))
        .collect()
}

pub fn media(id: i64) -> Result<AniListMedia> {
    let v = query(MEDIA_GQL, json!({ "id": id }))?;
    let media = &v["data"]["Media"];
    if !media.is_object() {
        bail!("AniList has no anime with id {id}");
    }
    Ok(parse_media(media))
}

pub fn trending(n: usize) -> Result<Vec<AniListMedia>> {
    let v = query(TRENDING_GQL, json!({ "n": n.clamp(1, 50) }))?;
    let media = v["data"]["Page"]["media"]
        .as_array()
        .context("AniList trending: missing media list")?;
    Ok(media.iter().map(parse_media).collect())
}

pub fn schedule(from_ts: i64, to_ts: i64) -> Result<Vec<Airing>> {
    let mut all = Vec::new();
    for page in 1..=SCHEDULE_MAX_PAGES {
        let v = query(
            SCHEDULE_GQL,
            json!({ "page": page, "from": from_ts, "to": to_ts }),
        )?;
        let (items, has_next) = parse_schedule(&v);
        all.extend(items);
        if !has_next {
            break;
        }
    }
    Ok(all)
}

pub fn recommendations(ids: &[i64]) -> Result<Vec<AniListMedia>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let ids = &ids[..ids.len().min(50)];
    let v = query(RECOMMENDATIONS_GQL, json!({ "ids": ids }))?;
    Ok(parse_recommendations(&v, ids))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn parse_media_strips_html() {
        let m = parse_media(&fixture("anilist_media.json")["data"]["Media"]);
        assert_eq!(m.id, 154587);
        assert_eq!(m.title_romaji, "Sousou no Frieren");
        assert_eq!(
            m.title_english.as_deref(),
            Some("Frieren: Beyond Journey’s End")
        );
        assert_eq!(
            m.cover_url,
            "https://s4.anilist.co/file/anilistcdn/media/anime/cover/large/bx154587-qQTzQnEJJ3oB.jpg"
        );
        assert_eq!(
            m.banner_url.as_deref(),
            Some(
                "https://s4.anilist.co/file/anilistcdn/media/anime/banner/154587-ivXNJ23SM1xB.jpg"
            )
        );
        let synopsis = m.synopsis.unwrap();
        assert!(synopsis.starts_with("The adventure is over"));
        assert!(synopsis.ends_with("new adventure…\n\n(Source: Crunchyroll)"));
        assert!(!synopsis.contains('<'));
        assert_eq!(m.genres, ["Adventure", "Drama", "Fantasy"]);
        assert_eq!(m.score, Some(90));
        assert_eq!(m.year, Some(2023));
        assert_eq!(m.format.as_deref(), Some("TV"));
        assert_eq!(m.status.as_deref(), Some("FINISHED"));
        assert_eq!(m.episodes, Some(28));
        assert_eq!(m.next_airing, None);
        assert_eq!(m.episode_thumbs.len(), 3);
        assert_eq!(m.episode_thumbs[0].0, "Episode 1 - The Journey's End");

        let m = parse_media(&json!({
            "id": 1,
            "title": { "romaji": "R", "english": null },
            "coverImage": { "extraLarge": null, "large": "https://x/l.jpg" },
            "description": "<b>Bold</b> &amp; <i>it&#039;s</i> &quot;fine&quot;<br>Next",
            "nextAiringEpisode": { "episode": 5, "airingAt": 1700000000 }
        }));
        assert_eq!(m.synopsis.as_deref(), Some("Bold & it's \"fine\"\nNext"));
        assert_eq!(m.cover_url, "https://x/l.jpg");
        assert_eq!(m.title_english, None);
        assert_eq!(m.next_airing, Some((5, 1_700_000_000)));
        assert!(m.genres.is_empty() && m.episode_thumbs.is_empty());
        assert_eq!(
            parse_media(&json!({ "description": " <br> " })).synopsis,
            None
        );
    }

    #[test]
    fn parse_schedule_items() {
        let (items, has_next) = parse_schedule(&fixture("anilist_schedule.json"));
        assert!(has_next);
        let summary: Vec<_> = items
            .iter()
            .map(|a| (a.media.id, a.episode, a.airing_at))
            .collect();
        assert_eq!(
            summary,
            [
                (207191, 8, 1_795_437_000),
                (209219, 8, 1_795_438_800),
                (160803, 8, 1_795_453_200)
            ]
        );
        assert_eq!(
            items[0].media.title_english.as_deref(),
            Some("Hello, I am a Witch and my Crush Wants me to Make a Love Potion!")
        );
        assert!(!items[0].media.synopsis.as_deref().unwrap().contains("<b>"));

        let (items, has_next) = parse_schedule(&json!({ "data": { "Page": {
            "pageInfo": { "hasNextPage": false },
            "airingSchedules": [{ "episode": 1, "airingAt": 5, "media": null }]
        } } }));
        assert!(items.is_empty() && !has_next);
    }

    #[test]
    fn recommendations_dedupe_and_exclude_seeds() {
        let rec = |id: i64| json!({ "mediaRecommendation": { "id": id, "title": { "romaji": format!("t{id}") } } });
        let v = json!({ "data": { "Page": { "media": [
            { "id": 2, "recommendations": { "nodes": [rec(30), rec(1), rec(10)] } },
            { "id": 1, "recommendations": { "nodes": [rec(10), { "mediaRecommendation": null }, rec(20)] } }
        ] } } });
        let ids: Vec<i64> = parse_recommendations(&v, &[1, 2])
            .iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, [10, 20, 30]);
    }
}
