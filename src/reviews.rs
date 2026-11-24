//! Public reviews of a show: AniList's own, and MyAnimeList's through the
//! unofficial Jikan API. Review text is untrusted user writing. It is reduced
//! to plain text here (HTML, markdown and embeds removed, spoilers split out)
//! and never rendered as markup.

use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::db::Db;
use crate::{anilist, catalog, net};

/// Reviews change slowly; a day keeps Jikan's quota untouched by browsing.
const TTL_SECS: i64 = 24 * 60 * 60;
/// How many reviews each source shows.
pub const PER_SOURCE: usize = 6;
/// Longest body kept per review, in characters. Some reviews run past
/// 20 000; the rest is a click away on the source site.
const MAX_BODY_CHARS: usize = 8_000;
const MAX_SUMMARY_CHARS: usize = 300;

const ANILIST_GQL: &str = "query ($id: Int) { Media(id: $id, type: ANIME) { \
    reviews(sort: RATING_DESC, perPage: 6) { nodes { summary body score rating \
    ratingAmount siteUrl user { name } } } } }";
const JIKAN_API: &str = "https://api.jikan.moe/v4";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ReviewSource {
    AniList,
    MyAnimeList,
}

impl ReviewSource {
    pub fn label(self) -> &'static str {
        match self {
            ReviewSource::AniList => "AniList",
            ReviewSource::MyAnimeList => "MyAnimeList",
        }
    }
}

/// A run of review text, hidden behind a click when it is a spoiler.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub text: String,
    pub spoiler: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Review {
    pub author: String,
    /// Out of 10, whatever scale the source uses.
    pub score: Option<f32>,
    /// (helpful votes, votes cast): AniList counts both, Jikan only gives
    /// a reaction total.
    pub helpful: Option<(u32, Option<u32>)>,
    pub summary: Option<String>,
    pub body: Vec<Segment>,
    /// The source's verdict tag, e.g. "Recommended" (MyAnimeList only).
    pub verdict: Option<String>,
    /// The review on its site, only ever an https AniList/MyAnimeList URL.
    pub url: Option<String>,
    /// Whether the body was cut at `MAX_BODY_CHARS`.
    pub shortened: bool,
}

impl Review {
    pub fn has_spoilers(&self) -> bool {
        self.body.iter().any(|s| s.spoiler)
    }
}

/// AniList's most helpful reviews of `anilist_id`, cached for a day.
pub fn anilist(db: &Mutex<Db>, anilist_id: i64) -> Result<Vec<Review>> {
    catalog::fetch_cached(
        db,
        &format!("anilist:v2:reviews:{anilist_id}"),
        TTL_SECS,
        || {
            let v = anilist::query_plain(ANILIST_GQL, json!({ "id": anilist_id }))?;
            Ok(parse_anilist(&v))
        },
    )
}

/// MyAnimeList's most reacted-to reviews of `mal_id` via Jikan, cached for
/// a day and paced to Jikan's limits.
pub fn myanimelist(db: &Mutex<Db>, mal_id: i64) -> Result<Vec<Review>> {
    catalog::fetch_cached(db, &format!("jikan:v1:reviews:{mal_id}"), TTL_SECS, || {
        JIKAN_PACE.wait();
        let url = format!("{JIKAN_API}/anime/{mal_id}/reviews?spoilers=true");
        let bytes = net::get_bytes(&url, &[("Accept", "application/json")])?;
        let v: Value =
            serde_json::from_slice(&bytes).with_context(|| format!("decoding JSON from {url}"))?;
        Ok(parse_jikan(&v))
    })
}

pub fn parse_anilist(v: &Value) -> Vec<Review> {
    v["data"]["Media"]["reviews"]["nodes"]
        .as_array()
        .map(|nodes| {
            nodes
                .iter()
                .filter_map(|n| {
                    let (body, shortened) = segments(n["body"].as_str()?);
                    Some(Review {
                        author: author(&n["user"]["name"]),
                        score: n["score"]
                            .as_u64()
                            .filter(|s| *s <= 100)
                            .map(|s| s as f32 / 10.0),
                        helpful: n["rating"]
                            .as_u64()
                            .map(|up| (clamp_u32(up), n["ratingAmount"].as_u64().map(clamp_u32))),
                        summary: n["summary"].as_str().and_then(summary),
                        body,
                        verdict: None,
                        url: site_url(&n["siteUrl"], "https://anilist.co/"),
                        shortened,
                    })
                })
                .filter(|r| !r.body.is_empty())
                .take(PER_SOURCE)
                .collect()
        })
        .unwrap_or_default()
}

pub fn parse_jikan(v: &Value) -> Vec<Review> {
    let mut reviews: Vec<(u64, Review)> = v["data"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|r| {
                    let raw = r["review"].as_str()?;
                    let (mut body, shortened) = segments(raw);
                    // Jikan flags whole reviews; hide all of one so flagged.
                    if r["is_spoiler"].as_bool() == Some(true) {
                        body.iter_mut().for_each(|s| s.spoiler = true);
                    }
                    let reactions = r["reactions"]["overall"].as_u64();
                    Some((
                        reactions.unwrap_or(0),
                        Review {
                            author: author(&r["user"]["username"]),
                            score: r["score"]
                                .as_u64()
                                .filter(|s| (1..=10).contains(s))
                                .map(|s| s as f32),
                            helpful: reactions.map(|n| (clamp_u32(n), None)),
                            summary: None,
                            body,
                            verdict: r["tags"]
                                .as_array()
                                .and_then(|t| t.iter().find_map(|t| t.as_str()))
                                .map(|t| clip(t.trim(), 40))
                                .filter(|t| !t.is_empty()),
                            url: site_url(&r["url"], "https://myanimelist.net/"),
                            shortened,
                        },
                    ))
                })
                .filter(|(_, r)| !r.body.is_empty())
                .collect()
        })
        .unwrap_or_default();
    // Most reacted-to first, like MyAnimeList's "most helpful" order.
    reviews.sort_by_key(|(reactions, _)| std::cmp::Reverse(*reactions));
    reviews
        .into_iter()
        .map(|(_, r)| r)
        .take(PER_SOURCE)
        .collect()
}

fn clamp_u32(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn author(v: &Value) -> String {
    v.as_str()
        .map(|s| clip(s.trim(), 40))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Anonymous".into())
}

/// `v` when it is an https URL on `site`; anything else is dropped, since
/// it may be opened in the browser.
fn site_url(v: &Value, site: &str) -> Option<String> {
    v.as_str()
        .filter(|u| u.starts_with(site) && u.len() < 300)
        .filter(|u| !u.chars().any(|c| c.is_whitespace() || c.is_control()))
        .map(str::to_string)
}

fn summary(raw: &str) -> Option<String> {
    let plain = clean(raw)?.split_whitespace().collect::<Vec<_>>().join(" ");
    (!plain.is_empty()).then(|| clip(&plain, MAX_SUMMARY_CHARS))
}

/// The first `max` characters of `s`, with "…" when cut.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut cut: String = s.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// The review body as plain text runs, spoilers (AniList's `~!…!~`)
/// separated out, capped at `MAX_BODY_CHARS`; and whether it was cut.
pub fn segments(raw: &str) -> (Vec<Segment>, bool) {
    let mut out: Vec<Segment> = Vec::new();
    let mut budget = MAX_BODY_CHARS;
    let mut shortened = false;
    for (piece, spoiler) in split_spoilers(raw) {
        let Some(text) = clean(piece) else {
            continue;
        };
        if budget == 0 {
            shortened = true;
            break;
        }
        let len = text.chars().count();
        let text = if len > budget {
            shortened = true;
            clip(&text, budget)
        } else {
            text
        };
        budget = budget.saturating_sub(len);
        match out.last_mut() {
            Some(last) if last.spoiler == spoiler => {
                last.text.push_str("\n\n");
                last.text.push_str(&text);
            }
            _ => out.push(Segment { text, spoiler }),
        }
    }
    (out, shortened)
}

/// (text, is_spoiler) runs of `raw`. An unclosed `~!` hides the rest.
fn split_spoilers(raw: &str) -> Vec<(&str, bool)> {
    let mut runs = Vec::new();
    let mut rest = raw;
    while let Some(open) = rest.find("~!") {
        runs.push((&rest[..open], false));
        let after = &rest[open + 2..];
        match after.find("!~") {
            Some(close) => {
                runs.push((&after[..close], true));
                rest = &after[close + 2..];
            }
            None => {
                runs.push((after, true));
                rest = "";
            }
        }
    }
    runs.push((rest, false));
    runs
}

static EMBED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:img|image|youtube|webm)\s*\d*%?\s*\([^)]*\)").expect("embed regex")
});
static LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]]*)\]\([^)]*\)").expect("link regex"));
static RULE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\s*_\-=~]{3,}$").expect("rule regex"));

/// Plain text from a review's mix of HTML and AniList markdown, or `None`
/// when nothing readable is left.
fn clean(raw: &str) -> Option<String> {
    let s = EMBED.replace_all(raw, "");
    let s = LINK.replace_all(&s, "$1");
    let s = s.replace("\r\n", "\n").replace("~~~", "").replace("~~", "");
    let s = anilist::strip_html(&s)?;
    let mut lines: Vec<String> = Vec::new();
    for line in s.lines() {
        let line = line.trim_start_matches(|c: char| c == '>' || c == '#' || c.is_whitespace());
        let line = if RULE.is_match(line) { "" } else { line };
        let bullet = line.strip_prefix("- ").or_else(|| line.strip_prefix("* "));
        let line = match bullet {
            Some(item) => format!("• {}", emphasis_free(item)),
            None => emphasis_free(line),
        };
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() && lines.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        lines.push(line);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// `line` without markdown emphasis: every `*` and backtick, and `_` that
/// opens or closes a word (`snake_case` keeps its underscore).
fn emphasis_free(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let word = |i: Option<usize>| {
        i.and_then(|i| chars.get(i))
            .is_some_and(|c| c.is_alphanumeric())
    };
    chars
        .iter()
        .enumerate()
        .filter(|&(i, &c)| match c {
            '*' | '`' => false,
            '_' => word(i.checked_sub(1)) && word(Some(i + 1)),
            _ => true,
        })
        .map(|(_, &c)| c)
        .collect()
}

/// Paces Jikan calls to its published limits (3 a second, 60 a minute),
/// with a little headroom.
struct Pace {
    sent: Mutex<VecDeque<Instant>>,
}

static JIKAN_PACE: Pace = Pace {
    sent: Mutex::new(VecDeque::new()),
};

const PER_SECOND: usize = 2;
const PER_MINUTE: usize = 50;

impl Pace {
    /// Blocks until a request may go out, then records it. Off the UI
    /// thread only.
    fn wait(&self) {
        loop {
            let delay = {
                let mut sent = self.sent.lock().unwrap_or_else(|e| e.into_inner());
                let now = Instant::now();
                match delay_needed(&mut sent, now) {
                    None => {
                        sent.push_back(now);
                        return;
                    }
                    Some(delay) => delay,
                }
            };
            std::thread::sleep(delay);
        }
    }
}

/// How long to wait before another request, given the send times in
/// `sent` (oldest first). Forgets sends older than a minute.
fn delay_needed(sent: &mut VecDeque<Instant>, now: Instant) -> Option<Duration> {
    const SECOND: Duration = Duration::from_secs(1);
    const MINUTE: Duration = Duration::from_secs(60);
    while sent
        .front()
        .is_some_and(|t| now.saturating_duration_since(*t) >= MINUTE)
    {
        sent.pop_front();
    }
    if sent.len() >= PER_MINUTE {
        return Some(MINUTE - now.saturating_duration_since(sent[sent.len() - PER_MINUTE]));
    }
    let recent = sent
        .iter()
        .filter(|t| now.saturating_duration_since(**t) < SECOND)
        .count();
    if recent >= PER_SECOND {
        let oldest = sent[sent.len() - PER_SECOND];
        return Some(SECOND - now.saturating_duration_since(oldest));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn plain(segments: &[Segment]) -> String {
        segments
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("|")
    }

    #[test]
    fn clean_strips_html_markdown_and_embeds() {
        let raw = ">#####___This review is spoiler-free.___\n\n~~~<p align=\"justify\"><h1>**A journey**</h1></p>~~~\n\
            ************\n~~~img220(https://media.example/x.gif)~~~\n\
            <hr><p>Have you *ever* felt _Frieren_ is [great](https://x.y)? snake_case &amp; more</p>\n\
            youtube(https://youtu.be/abc)\n- one\n* two\n`code`";
        assert_eq!(
            clean(raw).unwrap(),
            "This review is spoiler-free.\n\nA journey\n\nHave you ever felt Frieren is great? \
             snake_case & more\n\n• one\n• two\ncode"
        );
        assert_eq!(clean("~~~ <br> ~~~\n*****"), None);
        assert_eq!(
            clean("<script>alert(1)</script>ok").as_deref(),
            Some("alert(1)ok")
        );
    }

    #[test]
    fn spoilers_split_out() {
        let (segs, cut) = segments("Before ~!Himmel dies!~ after ~!unclosed tail");
        assert!(!cut);
        assert_eq!(
            segs,
            [
                Segment {
                    text: "Before".into(),
                    spoiler: false
                },
                Segment {
                    text: "Himmel dies".into(),
                    spoiler: true
                },
                Segment {
                    text: "after".into(),
                    spoiler: false
                },
                Segment {
                    text: "unclosed tail".into(),
                    spoiler: true
                },
            ]
        );
        // Adjacent runs of the same kind merge; empty spoilers vanish.
        let (segs, _) = segments("a ~! !~ b");
        assert_eq!(plain(&segs), "a\n\nb");
        assert!(segs.iter().all(|s| !s.spoiler));
    }

    #[test]
    fn long_bodies_are_capped() {
        let raw = "word ".repeat(4_000);
        let (segs, cut) = segments(&raw);
        assert!(cut);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text.chars().count(), MAX_BODY_CHARS);
        assert!(segs[0].text.ends_with('…'));
    }

    #[test]
    fn parse_anilist_reviews() {
        let reviews = parse_anilist(&fixture("anilist_reviews.json"));
        assert_eq!(reviews.len(), 3);
        let r = &reviews[0];
        assert_eq!(r.author, "befalt");
        assert_eq!(r.score, Some(10.0));
        assert_eq!(r.helpful, Some((336, Some(385))));
        assert_eq!(
            r.summary.as_deref(),
            Some("Fascinating, fulfilling, and full of feelings.")
        );
        assert_eq!(r.url.as_deref(), Some("https://anilist.co/review/24476"));
        assert!(!r.has_spoilers());
        assert!(plain(&r.body).starts_with("This review is spoiler-free."));

        let r = &reviews[1];
        assert_eq!(r.score, Some(6.5));
        assert!(r.has_spoilers());
        assert_eq!(r.body[1].text, "Himmel's statue scene.");
        // A non-AniList link is never kept.
        assert_eq!(r.url, None);
        assert_eq!(reviews[2].author, "Anonymous");
        assert_eq!(reviews[2].helpful, None);
        assert!(parse_anilist(&json!({ "data": { "Media": null } })).is_empty());
    }

    #[test]
    fn parse_jikan_reviews() {
        let reviews = parse_jikan(&fixture("jikan_reviews.json"));
        let authors: Vec<_> = reviews.iter().map(|r| r.author.as_str()).collect();
        // Most reactions first; the empty review is dropped.
        assert_eq!(authors, ["Lenovo", "kiri", "Shiro"]);
        let r = &reviews[0];
        assert_eq!(r.score, Some(10.0));
        assert_eq!(r.helpful, Some((1204, None)));
        assert_eq!(r.verdict.as_deref(), Some("Recommended"));
        assert_eq!(
            r.url.as_deref(),
            Some("https://myanimelist.net/reviews.php?id=500001")
        );
        assert!(!r.has_spoilers());
        assert_eq!(
            plain(&r.body),
            "Frieren is a quiet triumph.\n\nIt stays with you."
        );
        // `is_spoiler` hides the whole review.
        assert!(reviews[1].body.iter().all(|s| s.spoiler));
        assert_eq!(reviews[1].verdict.as_deref(), Some("Mixed Feelings"));
        assert_eq!(reviews[2].score, None);
        assert!(parse_jikan(&json!({ "status": 429 })).is_empty());
    }

    #[test]
    fn pace_holds_to_jikan_limits() {
        let start = Instant::now();
        let mut sent = VecDeque::new();
        assert_eq!(delay_needed(&mut sent, start), None);
        sent.push_back(start);
        sent.push_back(start + Duration::from_millis(100));
        let wait = delay_needed(&mut sent, start + Duration::from_millis(500)).unwrap();
        assert_eq!(wait, Duration::from_millis(500));
        assert_eq!(
            delay_needed(&mut sent, start + Duration::from_secs(1)),
            None
        );

        let mut sent: VecDeque<Instant> = (0..PER_MINUTE as u64)
            .map(|i| start + Duration::from_secs(i))
            .collect();
        let now = start + Duration::from_secs(PER_MINUTE as u64);
        assert_eq!(
            delay_needed(&mut sent, now),
            Some(Duration::from_secs(60 - PER_MINUTE as u64))
        );
        // A minute on, the oldest sends are forgotten.
        let later = start + Duration::from_secs(120);
        assert_eq!(delay_needed(&mut sent, later), None);
        assert!(sent.is_empty());
    }
}
