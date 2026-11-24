//! Display text for a show's AniList metadata: the hover overlay on poster
//! cards and the facts on the show page. Everything is optional upstream, so
//! every builder leaves out what is unknown rather than showing blanks.

use crate::model::{AniListMedia, FuzzyDate, ShowSummary};
use crate::ui::calendar::{day_parts, local_day};
use crate::ui::view_models::{blurb, clamp_words, format_label};

/// About three lines of overlay text on a 160 px poster.
const CARD_SYNOPSIS_CHARS: usize = 96;
const CARD_GENRES: usize = 3;
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// What a poster card shows over its art.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CardMeta {
    /// Out of 10, e.g. "8.2".
    pub score: Option<String>,
    /// Year, format and length, e.g. ["2023", "TV", "28 eps"].
    pub facts: Vec<String>,
    /// e.g. "Airing · Ep 7 Fri", "Finished".
    pub status: Option<String>,
    /// Whether new episodes are still coming out.
    pub airing: bool,
    pub genres: Vec<String>,
    pub synopsis: Option<String>,
}

impl CardMeta {
    /// `now` and `tz_offset_secs` (east of UTC) place the next episode's day.
    pub fn from_media(media: &AniListMedia, now: i64, tz_offset_secs: i64) -> CardMeta {
        let mut facts = Vec::new();
        facts.extend(media.year.map(|y| y.to_string()));
        facts.extend(media.format.as_deref().map(format_label));
        let movie = media.format.as_deref() == Some("MOVIE");
        facts.extend(
            media
                .episodes
                .filter(|&n| n > 0 && !(movie && n == 1))
                .map(episodes_short),
        );
        CardMeta {
            score: media.score.and_then(score_label),
            facts,
            status: card_status(media, now, tz_offset_secs),
            airing: media.status.as_deref() == Some("RELEASING"),
            genres: media.genres.iter().take(CARD_GENRES).cloned().collect(),
            synopsis: media
                .synopsis
                .as_deref()
                .map(blurb)
                .filter(|s| !s.is_empty())
                .map(|s| clamp_words(&s, CARD_SYNOPSIS_CHARS)),
        }
    }

    /// For a show only AllAnime knows: its episode counts.
    pub fn from_summary(show: &ShowSummary) -> CardMeta {
        let mut facts = Vec::new();
        if show.sub_count > 0 {
            facts.push(format!("{} sub", show.sub_count));
        }
        if show.dub_count > 0 {
            facts.push(format!("{} dub", show.dub_count));
        }
        CardMeta {
            facts,
            ..CardMeta::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.score.is_none()
            && self.facts.is_empty()
            && self.status.is_none()
            && self.genres.is_empty()
            && self.synopsis.is_none()
    }
}

/// AniList's 0–100 score out of 10 ("8.2"); `None` for 0, which AniList
/// uses for "not enough ratings".
pub fn score_label(score: u8) -> Option<String> {
    (1..=100)
        .contains(&score)
        .then(|| format!("{}.{}", score / 10, score % 10))
}

fn episodes_short(n: u32) -> String {
    if n == 1 {
        "1 ep".into()
    } else {
        format!("{n} eps")
    }
}

/// "Airing · Ep 7 Fri" (or "today"/"tomorrow"); other statuses by name.
fn card_status(media: &AniListMedia, now: i64, tz_offset_secs: i64) -> Option<String> {
    let status = media.status.as_deref()?;
    if status != "RELEASING" {
        return Some(status_label(status));
    }
    match media.next_airing.filter(|&(_, at)| at > now) {
        Some((ep, at)) => {
            let day = local_day(at, tz_offset_secs);
            let when = match day - local_day(now, tz_offset_secs) {
                0 => "today".to_string(),
                1 => "tomorrow".to_string(),
                _ => day_parts(day).0.to_string(),
            };
            Some(format!("Airing · Ep {ep} {when}"))
        }
        None => Some("Airing".into()),
    }
}

/// AniList's media status for display.
pub fn status_label(status: &str) -> String {
    match status {
        "RELEASING" => "Airing".into(),
        "FINISHED" => "Finished".into(),
        "NOT_YET_RELEASED" => "Upcoming".into(),
        "CANCELLED" => "Cancelled".into(),
        "HIATUS" => "On hiatus".into(),
        other => sentence_case(other),
    }
}

/// "LIGHT_NOVEL" as "Light novel".
pub fn sentence_case(constant: &str) -> String {
    let lower = constant.replace('_', " ").to_lowercase();
    let mut chars = lower.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The show page's metadata strip and facts.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ShowMeta {
    /// Out of 10, e.g. "9.0".
    pub score: Option<String>,
    /// The show's best rating rank, e.g. "#1 highest rated all time".
    pub score_rank: Option<String>,
    /// Format, season and length, e.g. ["TV", "Fall 2023", "28 eps × 24 min"].
    pub strip: Vec<String>,
    /// "Finished", or "Airing" with the next episode's countdown.
    pub status: Option<String>,
    pub airing: bool,
    /// (label, value) rows: studio, source, dates, popularity.
    pub facts: Vec<(&'static str, String)>,
}

impl ShowMeta {
    pub fn from_media(media: &AniListMedia, now: i64) -> ShowMeta {
        let mut strip = Vec::new();
        strip.extend(media.format.as_deref().map(format_label));
        let season = match (media.season.as_deref(), media.year) {
            (Some(season), Some(year)) => Some(format!("{} {year}", sentence_case(season))),
            (None, Some(year)) => Some(year.to_string()),
            _ => None,
        };
        strip.extend(season);
        strip.extend(length_label(media));

        let airing = media.status.as_deref() == Some("RELEASING");
        let status = media.status.as_deref().map(|s| {
            match media.next_airing.filter(|&(_, at)| airing && at > now) {
                Some((ep, at)) => format!("Airing · Ep {ep} in {}", countdown(at - now)),
                None => status_label(s),
            }
        });

        let mut facts: Vec<(&'static str, String)> = Vec::new();
        if !media.studios.is_empty() {
            let label = if media.studios.len() == 1 {
                "Studio"
            } else {
                "Studios"
            };
            facts.push((label, media.studios.join(", ")));
        }
        if let Some(source) = media.source.as_deref() {
            facts.push(("Source", sentence_case(source)));
        }
        if let Some(aired) = aired_label(media) {
            facts.push(("Aired", aired));
        }
        if let Some(popular) = popularity_label(media) {
            facts.push(("Popularity", popular));
        }
        if let Some(favs) = media.favourites.filter(|&f| f > 0) {
            facts.push(("Favourites", thousands(favs)));
        }
        if let Some(mean) = media.mean_score.and_then(score_label)
            && media.mean_score != media.score
        {
            facts.push(("Mean score", mean));
        }

        ShowMeta {
            score: media.score.and_then(score_label),
            score_rank: rank_label(media, true),
            strip,
            status,
            airing,
            facts,
        }
    }
}

/// "28 eps × 24 min", "28 episodes", or a film's running time.
fn length_label(media: &AniListMedia) -> Option<String> {
    let movie = media.format.as_deref() == Some("MOVIE");
    match (media.episodes.filter(|&n| n > 0), media.duration) {
        (Some(1) | None, Some(mins)) if movie => Some(running_time(mins)),
        (Some(n), Some(mins)) => Some(format!("{} × {mins} min", episodes_short(n))),
        (Some(1), None) => Some("1 episode".into()),
        (Some(n), None) => Some(format!("{n} episodes")),
        (None, Some(mins)) => Some(format!("{mins} min episodes")),
        (None, None) => None,
    }
}

/// "1 h 54 min", "45 min".
fn running_time(mins: u32) -> String {
    match (mins / 60, mins % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

/// "2 d 4 h", "5 h 10 min", "12 min".
pub fn countdown(secs: i64) -> String {
    let mins = (secs.max(0) + 59) / 60;
    let (days, hours, mins) = (mins / 1440, mins % 1440 / 60, mins % 60);
    match (days, hours) {
        (0, 0) => format!("{mins} min"),
        (0, h) => format!("{h} h {mins} min"),
        (d, 0) => format!("{d} d"),
        (d, h) => format!("{d} d {h} h"),
    }
}

/// "Sep 29, 2023 – Mar 22, 2024", "Since Apr 4, 2025", "Sep 29, 2023".
fn aired_label(media: &AniListMedia) -> Option<String> {
    let start = media.start_date.map(date_label)?;
    match (media.end_date.map(date_label), media.status.as_deref()) {
        (Some(end), _) if end != start => Some(format!("{start} – {end}")),
        (Some(_), _) => Some(start),
        (None, Some("RELEASING" | "HIATUS")) => Some(format!("Since {start}")),
        (None, Some("NOT_YET_RELEASED")) => Some(format!("Starts {start}")),
        (None, _) => Some(start),
    }
}

/// "Sep 29, 2023", "Sep 2023" or "2023", as much as is known.
pub fn date_label(date: FuzzyDate) -> String {
    let year = date.year.map(|y| y.to_string()).unwrap_or_default();
    let month = date
        .month
        .and_then(|m| MONTHS.get(usize::from(m).wrapping_sub(1)));
    match (month, date.day) {
        (Some(month), Some(day)) => format!("{month} {day}, {year}"),
        (Some(month), None) => format!("{month} {year}"),
        _ => year,
    }
}

/// "#50 all time · 501,920 lists", or whichever half is known.
fn popularity_label(media: &AniListMedia) -> Option<String> {
    let rank = media
        .rankings
        .iter()
        .find(|r| !r.rated && r.all_time)
        .map(|r| format!("#{} all time", r.rank));
    let members = media
        .popularity
        .filter(|&p| p > 0)
        .map(|p| format!("{} lists", thousands(p)));
    match (rank, members) {
        (Some(rank), Some(members)) => Some(format!("{rank} · {members}")),
        (rank, members) => rank.or(members),
    }
}

/// The show's headline rank of the `rated` kind: all-time if it has one,
/// else the best yearly or seasonal one, e.g. "#1 highest rated, Fall 2023".
fn rank_label(media: &AniListMedia, rated: bool) -> Option<String> {
    let ranks = media.rankings.iter().filter(|r| r.rated == rated);
    let best = ranks
        .clone()
        .find(|r| r.all_time)
        .or_else(|| ranks.min_by_key(|r| (r.rank, r.season.is_some())))?;
    let when = match (&best.season, best.year, best.all_time) {
        (_, _, true) => String::new(),
        (Some(season), Some(year), _) => format!(", {} {year}", sentence_case(season)),
        (None, Some(year), _) => format!(", {year}"),
        _ => String::new(),
    };
    Some(format!("#{} {}{when}", best.rank, best.context))
}

/// `n` with thousands separators: 501920 as "501,920".
pub fn thousands(n: u32) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Ranking;

    const MON_NOON_UTC: i64 = 1_795_435_200; // 2026-11-23 12:00 UTC, a Monday
    const DAY: i64 = 86_400;

    fn frieren() -> AniListMedia {
        AniListMedia {
            id: 154587,
            title_romaji: "Sousou no Frieren".into(),
            synopsis: Some(
                "The adventure is over but life goes on for an elf mage just beginning to learn \
                 what living is all about.\n\n(Source: Crunchyroll)"
                    .into(),
            ),
            genres: vec![
                "Adventure".into(),
                "Drama".into(),
                "Fantasy".into(),
                "Slice of Life".into(),
            ],
            score: Some(90),
            mean_score: Some(91),
            year: Some(2023),
            format: Some("TV".into()),
            status: Some("FINISHED".into()),
            episodes: Some(28),
            season: Some("FALL".into()),
            start_date: Some(FuzzyDate {
                year: Some(2023),
                month: Some(9),
                day: Some(29),
            }),
            end_date: Some(FuzzyDate {
                year: Some(2024),
                month: Some(3),
                day: Some(22),
            }),
            duration: Some(24),
            source: Some("MANGA".into()),
            studios: vec!["MADHOUSE".into()],
            popularity: Some(501_920),
            favourites: Some(57_272),
            rankings: vec![
                Ranking {
                    rank: 2,
                    rated: true,
                    all_time: false,
                    season: Some("FALL".into()),
                    year: Some(2023),
                    context: "highest rated".into(),
                },
                Ranking {
                    rank: 1,
                    rated: true,
                    all_time: true,
                    season: None,
                    year: None,
                    context: "highest rated all time".into(),
                },
                Ranking {
                    rank: 50,
                    rated: false,
                    all_time: true,
                    season: None,
                    year: None,
                    context: "most popular all time".into(),
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn card_meta_from_full_media() {
        let meta = CardMeta::from_media(&frieren(), MON_NOON_UTC, 0);
        assert_eq!(meta.score.as_deref(), Some("9.0"));
        assert_eq!(meta.facts, ["2023", "TV", "28 eps"]);
        assert_eq!(meta.status.as_deref(), Some("Finished"));
        assert!(!meta.airing);
        assert_eq!(meta.genres, ["Adventure", "Drama", "Fantasy"]);
        let synopsis = meta.synopsis.unwrap();
        assert!(synopsis.starts_with("The adventure is over"));
        assert!(synopsis.ends_with('…') && synopsis.chars().count() <= CARD_SYNOPSIS_CHARS);
        assert!(!synopsis.contains("Source"));
    }

    #[test]
    fn card_status_names_the_next_airing_day() {
        let mut m = AniListMedia {
            status: Some("RELEASING".into()),
            next_airing: Some((7, MON_NOON_UTC + 4 * DAY)),
            ..Default::default()
        };
        let status = |m: &AniListMedia, tz| CardMeta::from_media(m, MON_NOON_UTC, tz).status;
        assert_eq!(status(&m, 0).as_deref(), Some("Airing · Ep 7 Fri"));
        m.next_airing = Some((7, MON_NOON_UTC + 3600));
        assert_eq!(status(&m, 0).as_deref(), Some("Airing · Ep 7 today"));
        // 13:00 UTC is already Tuesday 01:00 at UTC+12.
        assert_eq!(
            status(&m, 12 * 3600).as_deref(),
            Some("Airing · Ep 7 today")
        );
        m.next_airing = Some((7, MON_NOON_UTC + DAY));
        assert_eq!(status(&m, 0).as_deref(), Some("Airing · Ep 7 tomorrow"));
        // A stale cache's next episode may already have aired.
        m.next_airing = Some((7, MON_NOON_UTC - 60));
        assert_eq!(status(&m, 0).as_deref(), Some("Airing"));
        assert!(CardMeta::from_media(&m, MON_NOON_UTC, 0).airing);
    }

    #[test]
    fn card_meta_leaves_out_unknowns() {
        let bare = CardMeta::from_media(&AniListMedia::default(), MON_NOON_UTC, 0);
        assert!(bare.is_empty());
        let movie = AniListMedia {
            format: Some("MOVIE".into()),
            episodes: Some(1),
            score: Some(0),
            synopsis: Some("(Source: X)".into()),
            ..Default::default()
        };
        let meta = CardMeta::from_media(&movie, MON_NOON_UTC, 0);
        assert_eq!(meta.facts, ["Movie"]);
        assert_eq!((meta.score, meta.synopsis), (None, None));

        let show = ShowSummary {
            sub_count: 12,
            dub_count: 0,
            ..Default::default()
        };
        assert_eq!(CardMeta::from_summary(&show).facts, ["12 sub"]);
        assert!(CardMeta::from_summary(&ShowSummary::default()).is_empty());
    }

    #[test]
    fn show_meta_from_full_media() {
        let meta = ShowMeta::from_media(&frieren(), MON_NOON_UTC);
        assert_eq!(meta.score.as_deref(), Some("9.0"));
        assert_eq!(
            meta.score_rank.as_deref(),
            Some("#1 highest rated all time")
        );
        assert_eq!(meta.strip, ["TV", "Fall 2023", "28 eps × 24 min"]);
        assert_eq!(meta.status.as_deref(), Some("Finished"));
        assert_eq!(
            meta.facts,
            [
                ("Studio", "MADHOUSE".to_string()),
                ("Source", "Manga".to_string()),
                ("Aired", "Sep 29, 2023 – Mar 22, 2024".to_string()),
                ("Popularity", "#50 all time · 501,920 lists".to_string()),
                ("Favourites", "57,272".to_string()),
                ("Mean score", "9.1".to_string()),
            ]
        );
    }

    #[test]
    fn show_meta_airing_and_sparse() {
        let mut m = AniListMedia {
            format: Some("TV".into()),
            status: Some("RELEASING".into()),
            next_airing: Some((8, MON_NOON_UTC + 2 * DAY + 4 * 3600)),
            start_date: Some(FuzzyDate {
                year: Some(2026),
                month: Some(10),
                day: None,
            }),
            year: Some(2026),
            rankings: vec![
                Ranking {
                    rank: 9,
                    rated: true,
                    year: Some(2026),
                    context: "highest rated".into(),
                    ..Default::default()
                },
                Ranking {
                    rank: 3,
                    rated: true,
                    season: Some("FALL".into()),
                    year: Some(2026),
                    context: "highest rated".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let meta = ShowMeta::from_media(&m, MON_NOON_UTC);
        assert_eq!(meta.status.as_deref(), Some("Airing · Ep 8 in 2 d 4 h"));
        assert!(meta.airing);
        assert_eq!(meta.strip, ["TV", "2026"]);
        assert_eq!(meta.facts, [("Aired", "Since Oct 2026".to_string())]);
        assert_eq!(
            meta.score_rank.as_deref(),
            Some("#3 highest rated, Fall 2026")
        );

        m.format = Some("MOVIE".into());
        m.episodes = Some(1);
        m.duration = Some(114);
        assert_eq!(
            ShowMeta::from_media(&m, MON_NOON_UTC).strip[2],
            "1 h 54 min"
        );
        assert_eq!(
            ShowMeta::from_media(&AniListMedia::default(), 0),
            ShowMeta::default()
        );
    }

    #[test]
    fn small_formatters() {
        assert_eq!(score_label(82).as_deref(), Some("8.2"));
        assert_eq!(score_label(100).as_deref(), Some("10.0"));
        assert_eq!(score_label(0), None);
        assert_eq!(score_label(101), None);
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(sentence_case("LIGHT_NOVEL"), "Light novel");
        assert_eq!(sentence_case(""), "");
        assert_eq!(countdown(59), "1 min");
        assert_eq!(countdown(3 * 3600 + 600), "3 h 10 min");
        assert_eq!(countdown(2 * 86_400), "2 d");
        let date = |year, month, day| FuzzyDate { year, month, day };
        assert_eq!(
            date_label(date(Some(2024), Some(3), Some(2))),
            "Mar 2, 2024"
        );
        assert_eq!(date_label(date(Some(2024), Some(13), None)), "2024");
        assert_eq!(date_label(date(Some(2024), None, Some(2))), "2024");
    }
}
