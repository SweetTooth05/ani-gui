use anyhow::Result;
use serde_json::{Value, json};

use crate::model::{Mode, ShowSummary};
use crate::net;

// Mirrors the constants inside the ani-cli script.
pub const API: &str = "https://api.allanime.day/api";
pub const REFERER: &str = "https://youtu-chan.com";
pub const AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:150.0) Gecko/20100101 Firefox/150.0";
pub const COVER_CDN: &str = "https://wp.youtube-anime.com/aln.youtube-anime.com";

const SEARCH_GQL: &str = "query( $search: SearchInput $limit: Int $page: Int \
    $translationType: VaildTranslationTypeEnumType \
    $countryOrigin: VaildCountryOriginEnumType ) { shows( \
    search: $search limit: $limit page: $page \
    translationType: $translationType countryOrigin: $countryOrigin ) \
    { edges { _id name englishName thumbnail aniListId availableEpisodes }}}";

const EPISODES_GQL: &str = "query ($showId: String!) { show( _id: $showId ) \
    { _id name thumbnail availableEpisodesDetail }}";

fn post(body: &Value) -> Result<Value> {
    net::post_json(API, body, &[("User-Agent", AGENT), ("Referer", REFERER)])
}

/// Keeps only shows with episodes in `mode`, in API order, so positions match
/// how `ani-cli -S <n>` numbers its results.
pub fn parse_search(v: &Value, mode: Mode) -> Vec<ShowSummary> {
    let Some(edges) = v["data"]["shows"]["edges"].as_array() else {
        return Vec::new();
    };
    edges
        .iter()
        .filter_map(|e| {
            let count = |m: Mode| {
                e["availableEpisodes"][m.as_str()]
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .unwrap_or(0)
            };
            let show = ShowSummary {
                id: e["_id"].as_str()?.to_string(),
                title: e["name"].as_str().unwrap_or_default().replace("\\\"", "\""),
                english_title: e["englishName"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                cover_url: cover_url(e["thumbnail"].as_str().unwrap_or_default()),
                anilist_id: match &e["aniListId"] {
                    Value::String(s) => s.parse().ok(),
                    other => other.as_i64(),
                },
                sub_count: count(Mode::Sub),
                dub_count: count(Mode::Dub),
            };
            (show.count(mode) > 0).then_some(show)
        })
        .collect()
}

fn cover_url(thumbnail: &str) -> String {
    if thumbnail.is_empty() || thumbnail.starts_with("http") {
        thumbnail.to_string()
    } else {
        format!("{COVER_CDN}/{}", thumbnail.trim_start_matches('/'))
    }
}

pub fn search(query: &str, mode: Mode) -> Result<Vec<ShowSummary>> {
    let body = json!({
        "variables": {
            "search": { "allowAdult": false, "allowUnknown": false, "query": query },
            "limit": 40,
            "page": 1,
            "translationType": mode.as_str(),
            "countryOrigin": "ALL",
        },
        "query": SEARCH_GQL,
    });
    Ok(parse_search(&post(&body)?, mode))
}

/// Numeric order; episodes that are not numbers go last in API order.
pub fn parse_episodes(v: &Value, mode: Mode) -> Vec<String> {
    let mut eps: Vec<String> = v["data"]["show"]["availableEpisodesDetail"][mode.as_str()]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let key = |ep: &str| {
        ep.parse::<f64>()
            .ok()
            .filter(|n| n.is_finite())
            .unwrap_or(f64::INFINITY)
    };
    eps.sort_by(|a, b| key(a).total_cmp(&key(b)));
    eps
}

pub fn episodes(show_id: &str, mode: Mode) -> Result<Vec<String>> {
    let body = json!({ "variables": { "showId": show_id }, "query": EPISODES_GQL });
    Ok(parse_episodes(&post(&body)?, mode))
}

/// 1-based position of `show_id` in `results`.
pub fn nth_of(results: &[ShowSummary], show_id: &str) -> Option<usize> {
    results.iter().position(|s| s.id == show_id).map(|i| i + 1)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn edges(edges: Value) -> Value {
        json!({ "data": { "shows": { "edges": edges } } })
    }

    #[test]
    fn parse_search_maps_fields() {
        let shows = parse_search(&fixture("allanime_search.json"), Mode::Sub);
        assert_eq!(shows.len(), 3);
        assert_eq!(
            shows[0],
            ShowSummary {
                id: "qpeexkeTa7DzLjRnp".into(),
                title: "Sousou no Frieren 2nd Season".into(),
                english_title: Some("Frieren: Beyond Journey’s End Season 2".into()),
                cover_url: "https://s4.anilist.co/file/anilistcdn/media/anime/cover/large/bx182255-butzrqd4I0aC.jpg".into(),
                anilist_id: Some(182255),
                sub_count: 10,
                dub_count: 10,
            }
        );

        let other = parse_search(
            &edges(json!([{
                "_id": "x1",
                "name": "Say \\\"Hi\\\"",
                "englishName": "",
                "thumbnail": "mcovers/a_tbs/dhw/x1.png",
                "aniListId": 42,
                "availableEpisodes": { "sub": 3, "dub": 0, "raw": 0 }
            }, {
                "_id": "x2",
                "name": "No Cover",
                "englishName": null,
                "thumbnail": null,
                "aniListId": null,
                "availableEpisodes": { "sub": 1 }
            }])),
            Mode::Sub,
        );
        assert_eq!(other[0].title, "Say \"Hi\"");
        assert_eq!(other[0].english_title, None);
        assert_eq!(
            other[0].cover_url,
            "https://wp.youtube-anime.com/aln.youtube-anime.com/mcovers/a_tbs/dhw/x1.png"
        );
        assert_eq!(other[0].anilist_id, Some(42));
        assert_eq!(other[1].cover_url, "");
        assert_eq!(other[1].anilist_id, None);
        assert_eq!(other[1].dub_count, 0);
    }

    #[test]
    fn search_filters_zero_count_mode() {
        let v = fixture("allanime_search.json");
        let sub: Vec<_> = parse_search(&v, Mode::Sub)
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(
            sub,
            [
                "qpeexkeTa7DzLjRnp",
                "sG52nbcFo3PfLg6PD",
                "ReHMC7TQnch3C6z8j"
            ]
        );
        let dub: Vec<_> = parse_search(&v, Mode::Dub)
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(dub, ["qpeexkeTa7DzLjRnp", "ReHMC7TQnch3C6z8j"]);
        assert!(parse_search(&json!({}), Mode::Sub).is_empty());
    }

    #[test]
    fn parse_episodes_sorted_numeric() {
        let detail = |sub: Value| json!({ "data": { "show": { "availableEpisodesDetail": { "sub": sub } } } });
        assert_eq!(
            parse_episodes(&detail(json!(["10", "2", "1", "5.5"])), Mode::Sub),
            ["1", "2", "5.5", "10"]
        );
        assert_eq!(
            parse_episodes(&detail(json!(["special", "3", "NaN", "1"])), Mode::Sub),
            ["1", "3", "special", "NaN"]
        );
        assert!(parse_episodes(&detail(json!(["1"])), Mode::Dub).is_empty());

        let ten: Vec<String> = (1..=10).map(|n| n.to_string()).collect();
        let v = fixture("allanime_show.json");
        assert_eq!(parse_episodes(&v, Mode::Sub), ten);
        assert_eq!(parse_episodes(&v, Mode::Dub), ten);
    }

    #[test]
    fn nth_of_is_one_based() {
        let shows = parse_search(&fixture("allanime_search.json"), Mode::Dub);
        assert_eq!(nth_of(&shows, "qpeexkeTa7DzLjRnp"), Some(1));
        assert_eq!(nth_of(&shows, "ReHMC7TQnch3C6z8j"), Some(2));
        assert_eq!(nth_of(&shows, "sG52nbcFo3PfLg6PD"), None);
    }

    #[test]
    #[ignore = "hits the live AllAnime API"]
    fn live_search_frieren() {
        let shows = search("frieren", Mode::Sub).unwrap();
        let frieren = shows
            .iter()
            .find(|s| s.anilist_id == Some(154587))
            .expect("Sousou no Frieren in results");
        assert!(frieren.sub_count >= 28);
        assert!(frieren.cover_url.starts_with("https://"));
        let eps = episodes(&frieren.id, Mode::Sub).unwrap();
        assert_eq!(eps.first().map(String::as_str), Some("1"));
    }
}
