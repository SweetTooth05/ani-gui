use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::anyhow;
use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Sub,
    Dub,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Sub => "sub",
            Mode::Dub => "dub",
        }
    }
}

impl FromStr for Mode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s {
            "sub" => Ok(Mode::Sub),
            "dub" => Ok(Mode::Dub),
            other => Err(anyhow!("unknown mode: {other:?}")),
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ShowSummary {
    pub id: String,
    pub title: String,
    pub english_title: Option<String>,
    pub cover_url: String,
    pub anilist_id: Option<i64>,
    pub sub_count: u32,
    pub dub_count: u32,
}

impl ShowSummary {
    pub fn count(&self, mode: Mode) -> u32 {
        match mode {
            Mode::Sub => self.sub_count,
            Mode::Dub => self.dub_count,
        }
    }

    pub fn display_title(&self) -> &str {
        self.english_title.as_deref().unwrap_or(&self.title)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AniListMedia {
    pub id: i64,
    pub title_romaji: String,
    pub title_english: Option<String>,
    pub cover_url: String,
    pub banner_url: Option<String>,
    /// Plain text; HTML tags are stripped when parsing.
    pub synopsis: Option<String>,
    pub genres: Vec<String>,
    pub score: Option<u8>,
    pub year: Option<u16>,
    pub format: Option<String>,
    pub status: Option<String>,
    pub episodes: Option<u32>,
    /// (episode, airing_at unix seconds)
    pub next_airing: Option<(u32, i64)>,
    /// (title, thumbnail url)
    pub episode_thumbs: Vec<(String, String)>,

    // Fields below arrived after `AniListMedia` was first stored as JSON
    // (`shows.media_json`, `api_cache`); each must default so old rows load.
    /// The MyAnimeList id, for Jikan lookups.
    #[serde(default)]
    pub id_mal: Option<i64>,
    /// Plain mean of user scores (0–100); `score` is AniList's weighted one.
    #[serde(default)]
    pub mean_score: Option<u8>,
    /// Users with the show on their list.
    #[serde(default)]
    pub popularity: Option<u32>,
    #[serde(default)]
    pub favourites: Option<u32>,
    /// "WINTER", "SPRING", "SUMMER" or "FALL".
    #[serde(default)]
    pub season: Option<String>,
    #[serde(default)]
    pub start_date: Option<FuzzyDate>,
    #[serde(default)]
    pub end_date: Option<FuzzyDate>,
    /// Minutes per episode.
    #[serde(default)]
    pub duration: Option<u32>,
    /// What it was adapted from ("MANGA", "LIGHT_NOVEL", "ORIGINAL", …).
    #[serde(default)]
    pub source: Option<String>,
    /// Main studios, in AniList's order.
    #[serde(default)]
    pub studios: Vec<String>,
    #[serde(default)]
    pub rankings: Vec<Ranking>,
}

/// An AniList date, any part of which may be unknown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FuzzyDate {
    pub year: Option<i32>,
    pub month: Option<u8>,
    pub day: Option<u8>,
}

/// One of AniList's chart positions, e.g. #12 most popular all time.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ranking {
    pub rank: u32,
    /// Ranked by score ("highest rated") rather than by popularity.
    pub rated: bool,
    pub all_time: bool,
    pub season: Option<String>,
    pub year: Option<u16>,
    /// AniList's wording, e.g. "most popular all time".
    pub context: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Airing {
    pub media: AniListMedia,
    pub episode: u32,
    pub airing_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stream {
    pub url: String,
    pub referrer: Option<String>,
    pub audio_file: Option<String>,
    pub sub_file: Option<String>,
    pub title: Option<String>,
}

/// Whether `ep` is a plain episode number (`^[0-9]+(\.[0-9]+)?$`). Only
/// these may reach ani-cli: it pastes the episode into a `sed` script, so
/// anything else could run commands.
pub fn valid_episode(ep: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    match ep.split_once('.') {
        Some((whole, frac)) => digits(whole) && digits(frac),
        None => digits(ep),
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EpisodeRequest {
    /// What ani-cli searches its provider for.
    pub query: String,
    /// Titles the menu shim accepts in ani-cli's "Select anime" menu.
    pub want_titles: Vec<String>,
    /// 1-based `-S` pick, once the user chose among ani-cli's results.
    pub select_index: Option<usize>,
    pub ep: String,
    pub mode: Mode,
    pub quality: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    pub show_id: String,
    pub mode: Mode,
    pub ep: String,
    pub position: f64,
    pub duration: f64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShowRecord {
    pub summary: ShowSummary,
    pub media: Option<AniListMedia>,
    pub updated_at: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DlStatus {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl DlStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            DlStatus::Queued => "queued",
            DlStatus::Running => "running",
            DlStatus::Done => "done",
            DlStatus::Failed => "failed",
            DlStatus::Cancelled => "cancelled",
        }
    }
}

impl FromStr for DlStatus {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s {
            "queued" => Ok(DlStatus::Queued),
            "running" => Ok(DlStatus::Running),
            "done" => Ok(DlStatus::Done),
            "failed" => Ok(DlStatus::Failed),
            "cancelled" => Ok(DlStatus::Cancelled),
            other => Err(anyhow!("unknown download status: {other:?}")),
        }
    }
}

impl fmt::Display for DlStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DownloadRecord {
    pub id: String,
    pub show_id: String,
    pub title: String,
    pub ep: String,
    pub mode: Mode,
    pub quality: String,
    pub path: Option<String>,
    pub status: DlStatus,
    pub progress: f32,
    pub error: Option<String>,
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub mode: Mode,
    pub quality: String,
    pub download_dir: PathBuf,
    pub autoplay_next: bool,
    /// Install signed releases in place when ani-gui can update itself.
    pub auto_update: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            mode: Mode::Sub,
            quality: "best".to_string(),
            download_dir: paths::default_download_dir(),
            autoplay_next: true,
            auto_update: true,
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn valid_episode_accepts_only_plain_numbers() {
        for ok in ["1", "12", "0", "5.5", "100.25", "007"] {
            assert!(super::valid_episode(ok), "{ok:?}");
        }
        for bad in [
            "",
            " 1",
            "1 ",
            "1.",
            ".5",
            "1.2.3",
            "-1",
            "+1",
            "1e3",
            "inf",
            "NaN",
            "special",
            "１",
            "1\\|x$|touch FILE|e;#",
            "1\n2",
        ] {
            assert!(!super::valid_episode(bad), "{bad:?}");
        }
    }

    use super::*;

    #[test]
    fn mode_and_status_roundtrip_strings() {
        for mode in [Mode::Sub, Mode::Dub] {
            assert_eq!(mode.as_str().parse::<Mode>().ok(), Some(mode));
        }
        for status in [
            DlStatus::Queued,
            DlStatus::Running,
            DlStatus::Done,
            DlStatus::Failed,
            DlStatus::Cancelled,
        ] {
            assert_eq!(status.as_str().parse::<DlStatus>().ok(), Some(status));
        }
        assert!("SUB".parse::<Mode>().is_err());
        assert_eq!(Mode::default(), Mode::Sub);
    }

    #[test]
    fn summary_count_and_display_title() {
        let mut show = ShowSummary {
            title: "Sousou no Frieren".into(),
            sub_count: 28,
            dub_count: 10,
            ..Default::default()
        };
        assert_eq!(show.count(Mode::Sub), 28);
        assert_eq!(show.count(Mode::Dub), 10);
        assert_eq!(show.display_title(), "Sousou no Frieren");
        show.english_title = Some("Frieren: Beyond Journey's End".into());
        assert_eq!(show.display_title(), "Frieren: Beyond Journey's End");
    }
}
