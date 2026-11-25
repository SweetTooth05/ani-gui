use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError, Weak, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use sha2::{Digest, Sha256};

use crate::model::{EpisodeRequest, Mode, ShowSummary, Stream, valid_episode};

const RESOLVE_TIMEOUT: Duration = Duration::from_secs(60);
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
const SUBTITLE_TIMEOUT: Duration = Duration::from_secs(10);
/// Hard cap on downloaded/read subtitle files (network data is untrusted).
const MAX_SUBTITLE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SUB_CANDIDATES: usize = 6;
const MAX_CACHED_SUBS: usize = 32;
const SUBS_DIR: &str = "subs";
/// Matches ani-cli's browser UA so subtitle CDNs don't reject the fetch.
const SUBTITLE_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
/// How much of ani-cli's output an error message carries.
const DETAIL_CHARS: usize = 240;
const SHIMS_DIR: &str = "shims";
/// ani-cli runs the player as `$ANI_CLI_PLAYER`; a name containing "mpv"
/// makes it pass `--referrer=`, `--sub-file=`, … and the URL as arguments.
const CAPTURE_NAME: &str = "capture-mpv";
const CAPTURE_SHIM: &str = "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done\n";
const MENU_NAME: &str = "menu";
/// ani-cli switches to rofi or dmenu, found on PATH, when stdin is not a tty.
const MENU_ALIASES: [&str; 2] = ["rofi", "dmenu"];
/// The argument `main` dispatches to [`menu_main`].
pub const MENU_FLAG: &str = "--ani-cli-menu";
const NO_RESULTS: &str = "No results found";
/// Folders with ani-cli and its tools that a session started from a desktop
/// launcher may lack on PATH (besides `~/.local/bin`). On macOS an app
/// started from Finder gets only the system folders, so Homebrew's prefixes
/// (Apple Silicon, then Intel) are added.
const EXTRA_BIN_DIRS: &[&str] = if cfg!(target_os = "macos") {
    &["/opt/homebrew/bin", "/usr/local/bin"]
} else {
    &["/usr/local/bin"]
};

/// The `ani-cli` executable on PATH, else in `~/.local/bin` or one of
/// [`EXTRA_BIN_DIRS`].
pub fn find() -> Option<PathBuf> {
    let path = env::var_os("PATH").unwrap_or_default();
    env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .chain(env::home_dir().map(|home| home.join(".local/bin")))
        .chain(EXTRA_BIN_DIRS.iter().map(PathBuf::from))
        .map(|dir| dir.join("ani-cli"))
        .find(|p| fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// The error for a missing ani-cli, naming where [`find`] looked.
pub fn not_installed() -> String {
    let mut dirs: Vec<&str> = vec!["~/.local/bin"];
    dirs.extend(EXTRA_BIN_DIRS);
    let last = dirs.pop().unwrap_or_default();
    format!(
        "ani-cli is not installed (looked on PATH, in {} and {last})",
        dirs.join(", ")
    )
}

/// `ani-cli -V`, e.g. "5.1.8".
pub fn version(path: &Path) -> Option<String> {
    let out = run_capture(&[path.into(), "-V".into()], &[], VERSION_TIMEOUT, None).ok()?;
    let version = out.stdout.lines().next()?.trim();
    (out.status.success() && !version.is_empty()).then(|| version.to_string())
}

/// Writes `<cache_dir>/shims/`: the `capture-mpv` player, the `menu` script
/// running `exe --ani-cli-menu`, and `rofi`/`dmenu` links to it. Returns the
/// directory, which goes first on ani-cli's PATH.
pub fn ensure_shims(cache_dir: &Path, exe: &Path) -> io::Result<PathBuf> {
    let dir = cache_dir.join(SHIMS_DIR);
    fs::create_dir_all(&dir)?;
    write_script(&dir, CAPTURE_NAME, CAPTURE_SHIM.as_bytes())?;
    let mut menu = b"#!/bin/sh\nexec ".to_vec();
    menu.extend(sh_quote(exe.as_os_str().as_bytes()));
    menu.extend(format!(" {MENU_FLAG} \"$@\"\n").as_bytes());
    write_script(&dir, MENU_NAME, &menu)?;
    for alias in MENU_ALIASES {
        let link = dir.join(alias);
        if fs::read_link(&link).is_ok_and(|target| target == Path::new(MENU_NAME)) {
            continue;
        }
        let tmp = temp_name(&dir, alias);
        symlink(MENU_NAME, &tmp)?;
        replace(&tmp, &link)?;
    }
    Ok(dir)
}

/// Writes an executable script atomically unless it is already in place.
fn write_script(dir: &Path, name: &str, content: &[u8]) -> io::Result<()> {
    let path = dir.join(name);
    let mode = fs::metadata(&path).map(|m| m.permissions().mode() & 0o777);
    if mode.is_ok_and(|m| m == 0o755) && fs::read(&path).is_ok_and(|c| c == content) {
        return Ok(());
    }
    let tmp = temp_name(dir, name);
    fs::write(&tmp, content)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
    replace(&tmp, &path)
}

fn temp_name(dir: &Path, name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(".{name}.tmp{}-{n}", std::process::id()))
}

fn replace(tmp: &Path, path: &Path) -> io::Result<()> {
    fs::rename(tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(tmp);
    })
}

/// `s` in single quotes for `sh`.
fn sh_quote(s: &[u8]) -> Vec<u8> {
    let mut quoted = vec![b'\''];
    for &b in s {
        if b == b'\'' {
            quoted.extend(b"'\\''");
        } else {
            quoted.push(b);
        }
    }
    quoted.push(b'\'');
    quoted
}

#[derive(Clone, Debug, PartialEq)]
pub enum ResolveOutcome {
    Stream(Stream),
    /// No ani-cli result matched the show; its result titles, in its order,
    /// for the user to choose from (then re-run with `select_index`).
    NeedsPick(Vec<String>),
}

/// The ani-cli request for one episode of `show`: search for the remembered
/// provider title, else the English title, else the romaji one, accepting
/// any of them in the menu.
pub fn build_request(
    show: &ShowSummary,
    provider_title: Option<&str>,
    ep: &str,
    mode: Mode,
    quality: &str,
) -> EpisodeRequest {
    let mut want_titles: Vec<String> = Vec::new();
    let candidates = [
        provider_title,
        show.english_title.as_deref(),
        Some(&show.title),
    ];
    for title in candidates.into_iter().flatten() {
        if !title.is_empty() && !want_titles.iter().any(|t| t == title) {
            want_titles.push(title.to_string());
        }
    }
    EpisodeRequest {
        query: want_titles.first().cloned().unwrap_or_default(),
        want_titles,
        select_index: None,
        ep: ep.to_string(),
        mode,
        quality: quality.to_string(),
    }
}

/// Lowercase, `’` as `'`, punctuation dropped, whitespace collapsed.
pub fn normalize_title(s: &str) -> String {
    let kept: String = s
        .replace('’', "'")
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect();
    kept.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The menu program ani-cli runs (as `rofi -p <prompt> …`, `dmenu -p …` or
/// `menu … <prompt>`), given its arguments and the `N title` lines on stdin.
/// In "Select anime" it returns the first line whose title matches a wanted
/// title; with no match it writes the candidate titles as a JSON array to
/// `out_file` and returns nothing, as for any other menu.
pub fn menu_main(
    args: &[String],
    stdin: &str,
    want_titles: &[String],
    out_file: Option<&Path>,
) -> String {
    let prompt = args
        .iter()
        .position(|a| a == "-p")
        .and_then(|i| args.get(i + 1))
        .or(args.last())
        .map_or("", String::as_str);
    if !prompt.contains("Select anime") {
        return String::new();
    }
    let wanted: Vec<String> = want_titles
        .iter()
        .map(|t| normalize_title(t))
        .filter(|t| !t.is_empty())
        .collect();
    let lines: Vec<(&str, &str)> = stdin
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter_map(|l| Some((l, l.split_once(' ')?.1)))
        .collect();
    if let Some((line, _)) = lines
        .iter()
        .find(|(_, title)| wanted.contains(&normalize_title(title)))
    {
        return format!("{line}\n");
    }
    if let Some(out) = out_file {
        let titles: Vec<&str> = lines.iter().map(|(_, title)| *title).collect();
        let written = serde_json::to_string(&titles)
            .map_err(io::Error::from)
            .and_then(|json| fs::write(out, json));
        if let Err(err) = written {
            eprintln!("ani-gui: writing {}: {err}", out.display());
        }
    }
    String::new()
}

/// Reads what ani-cli handed the capture shim: one argument per line, among
/// ani-cli's own status lines. The URL is the last line starting with `http`.
/// When several `--sub-file=` arguments are present, prefers English full
/// subtitle tracks over signs/non-English/thumbnail tracks.
#[allow(dead_code)]
pub fn parse_capture(stdout: &str) -> Result<Stream> {
    Ok(parse_capture_with_subs(stdout)?.0)
}

/// Like [`parse_capture`], also returning every `--sub-file=` argument in the
/// order ani-cli passed them (deduplicated) so [`prepare_subtitles`] can
/// inspect, repair and compare candidates off the UI thread.
fn parse_capture_with_subs(stdout: &str) -> Result<(Stream, Vec<String>)> {
    let mut url = None;
    let mut referrer = None;
    let mut audio_file = None;
    let mut sub_files: Vec<String> = Vec::new();
    let mut title = None;
    for line in strip_ansi(stdout).lines().map(str::trim) {
        let value = |flag: &str| {
            line.strip_prefix(flag)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        if line.starts_with("--referrer=") {
            referrer = value("--referrer=");
        } else if line.starts_with("--audio-file=") {
            audio_file = value("--audio-file=");
        } else if line.starts_with("--sub-file=") {
            if let Some(sub) = value("--sub-file=")
                && !sub_files.contains(&sub)
            {
                sub_files.push(sub);
            }
        } else if line.starts_with("--force-media-title=") {
            title = value("--force-media-title=");
        } else if line.starts_with("http") {
            url = Some(line.to_string());
        }
    }
    let sub_file = select_best_sub_url(&sub_files).map(str::to_string);
    Ok((
        Stream {
            url: url.context("ani-cli returned no stream URL")?,
            referrer,
            audio_file,
            sub_file,
            title,
        },
        sub_files,
    ))
}

static SUBTITLE_AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .timeout_global(Some(SUBTITLE_TIMEOUT))
        .user_agent(SUBTITLE_USER_AGENT)
        .build()
        .into()
});

/// Language tier inferred from a subtitle URL/path: English > opaque/unknown >
/// clearly non-English.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum LangTier {
    Other = 0,
    Unknown = 1,
    English = 2,
}

/// Splits `s` into lowercase ASCII alphanumeric tokens for language/role hints.
fn url_tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn classify_lang_tokens(tokens: &[String]) -> Option<LangTier> {
    const ENGLISH: &[&str] = &["en", "eng", "english"];
    const OTHER: &[&str] = &[
        "ar",
        "ara",
        "arabic",
        "fr",
        "fre",
        "fra",
        "french",
        "de",
        "ger",
        "deu",
        "german",
        "it",
        "ita",
        "italian",
        "pt",
        "por",
        "portuguese",
        "ru",
        "rus",
        "russian",
        "es",
        "spa",
        "spanish",
        "id",
        "ind",
        "indonesian",
        "th",
        "tha",
        "thai",
        "vi",
        "vie",
        "vietnamese",
        "zh",
        "chi",
        "zho",
        "chinese",
        "ko",
        "kor",
        "korean",
        "ja",
        "jpn",
        "japanese",
        "tr",
        "tur",
        "turkish",
        "pl",
        "pol",
        "polish",
    ];
    if tokens.iter().any(|t| ENGLISH.contains(&t.as_str())) {
        Some(LangTier::English)
    } else if tokens.iter().any(|t| OTHER.contains(&t.as_str())) {
        Some(LangTier::Other)
    } else {
        None
    }
}

fn is_thumbnail_tokens(tokens: &[String]) -> bool {
    const THUMB: &[&str] = &[
        "thumb",
        "thumbs",
        "thumbnail",
        "thumbnails",
        "sprite",
        "sprites",
        "storyboard",
        "bif",
    ];
    tokens.iter().any(|t| THUMB.contains(&t.as_str()))
}

fn is_signs_tokens(tokens: &[String]) -> bool {
    const SIGNS: &[&str] = &["sign", "signs", "songs", "forced"];
    tokens.iter().any(|t| SIGNS.contains(&t.as_str()))
}

/// Language tier and offline URL preference for one subtitle URL/path.
/// Checks the filename before parent directories/query so a path like
/// `/en/subs/spanish.vtt` is classified as Spanish rather than English.
fn sub_url_meta(url: &str) -> (LangTier, bool, bool) {
    let without_frag = url.split('#').next().unwrap_or(url);
    let (path, query) = without_frag
        .split_once('?')
        .map_or((without_frag, ""), |(p, q)| (p, q));
    let filename = path.rsplit('/').next().unwrap_or(path);
    let file_tokens = url_tokens(filename);
    let query_tokens = url_tokens(query);
    let path_tokens = url_tokens(path);

    let thumb = is_thumbnail_tokens(&file_tokens) || is_thumbnail_tokens(&path_tokens);
    let signs = is_signs_tokens(&file_tokens) || is_signs_tokens(&query_tokens);
    let lang = classify_lang_tokens(&file_tokens)
        .or_else(|| classify_lang_tokens(&query_tokens))
        .or_else(|| classify_lang_tokens(&path_tokens))
        .unwrap_or(LangTier::Unknown);
    (lang, signs, thumb)
}

fn sub_url_rank(url: &str) -> i32 {
    let (lang, signs, thumb) = sub_url_meta(url);
    if thumb {
        return -100;
    }
    let mut score = match lang {
        LangTier::English => 30,
        LangTier::Unknown => 0,
        LangTier::Other => -30,
    };
    if signs {
        score -= 15;
    }
    score
}

/// Offline URL-only pick among `--sub-file=` candidates (first highest-ranked).
fn select_best_sub_url(urls: &[String]) -> Option<&str> {
    urls.iter()
        .enumerate()
        .max_by(|(ia, a), (ib, b)| sub_url_rank(a).cmp(&sub_url_rank(b)).then(ib.cmp(ia)))
        .map(|(_, u)| u.as_str())
}

/// Result of repairing a WebVTT file for FFmpeg/libmpv's `webvtt` demuxer.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SanitizedVtt {
    pub content: String,
    pub cue_count: usize,
    pub last_end_secs: f64,
    pub is_thumbnail: bool,
    pub repaired_orphans: usize,
}

/// Parses `MM:SS.mmm`, `HH:MM:SS.mmm` (or SRT `,` decimals / 1–2 digit ms)
/// into whole milliseconds.
fn parse_vtt_timestamp_ms(s: &str) -> Option<u64> {
    let (time, frac) = s.split_once('.').or_else(|| s.split_once(','))?;
    if frac.is_empty() || frac.len() > 3 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let ms: u64 = match frac.len() {
        1 => frac.parse::<u64>().ok()? * 100,
        2 => frac.parse::<u64>().ok()? * 10,
        _ => frac.parse::<u64>().ok()?,
    };
    let mut parts = time.split(':');
    let first = parts.next()?;
    let second = parts.next()?;
    let third = parts.next();
    if parts.next().is_some() {
        return None;
    }
    let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    let (h, m, sec) = match third {
        Some(s_part) => {
            if !digits(first) || second.len() != 2 || s_part.len() != 2 {
                return None;
            }
            let h: u64 = first.parse().ok()?;
            let m: u64 = second.parse().ok()?;
            let sec: u64 = s_part.parse().ok()?;
            if m >= 60 || sec >= 60 {
                return None;
            }
            (h, m, sec)
        }
        None => {
            if !digits(first) || second.len() != 2 {
                return None;
            }
            let m: u64 = first.parse().ok()?;
            let sec: u64 = second.parse().ok()?;
            if sec >= 60 {
                return None;
            }
            (0, m, sec)
        }
    };
    h.checked_mul(3_600_000)?
        .checked_add(m.checked_mul(60_000)?)?
        .checked_add(sec.checked_mul(1_000)?)?
        .checked_add(ms)
}

fn format_vtt_timestamp_ms(total_ms: u64) -> String {
    let hours = total_ms / 3_600_000;
    let mins = (total_ms / 60_000) % 60;
    let secs = (total_ms / 1_000) % 60;
    let ms = total_ms % 1_000;
    if hours > 0 {
        format!("{hours:02}:{mins:02}:{secs:02}.{ms:03}")
    } else {
        format!("{mins:02}:{secs:02}.{ms:03}")
    }
}

/// Parses a WebVTT cue timing line (`start --> end [settings]`), normalizing
/// timestamps to dot-decimal `MM:SS.mmm` / `HH:MM:SS.mmm` and clamping
/// inverted ranges (`end < start`).
fn parse_vtt_timing_line(line: &str) -> Option<(String, f64, f64)> {
    let (left, right) = line.split_once("-->")?;
    let start_ms = parse_vtt_timestamp_ms(left.trim())?;
    let right = right.trim_start();
    let end_len = right
        .find(|c: char| c.is_ascii_whitespace())
        .unwrap_or(right.len());
    let (end_token, settings) = right.split_at(end_len);
    let end_ms = parse_vtt_timestamp_ms(end_token)?.max(start_ms);
    let timing = format!(
        "{} --> {}{settings}",
        format_vtt_timestamp_ms(start_ms),
        format_vtt_timestamp_ms(end_ms)
    );
    Some((timing, start_ms as f64 / 1000.0, end_ms as f64 / 1000.0))
}

/// Repairs WebVTT subtitles so FFmpeg's `webvtt` demuxer (used by libmpv) does
/// not silently truncate mid-episode.
///
/// Upstream ASS→VTT converters frequently emit blank lines inside multi-line
/// signs/titles (e.g. `18:09.650 --> 18:12.610\n<b>Edible\n\nReadings</b>`).
/// Web browsers skip non-cue blocks per the WebVTT spec, but FFmpeg's
/// `webvtt_read_header` treats any non-header block lacking a timestamp as EOF
/// and silently drops every remaining cue in the episode. Reattaching orphan
/// blocks to the preceding cue restores both the multi-line sign and the rest
/// of the file.
pub(crate) fn sanitize_webvtt(raw: &str) -> SanitizedVtt {
    let normalized = raw
        .strip_prefix('\u{feff}')
        .unwrap_or(raw)
        .replace("\r\n", "\n")
        .replace('\r', "\n");

    let mut blocks: Vec<Vec<&str>> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    for line in normalized.split('\n') {
        if line.trim().is_empty() {
            if !cur.is_empty() {
                blocks.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(line);
        }
    }
    if !cur.is_empty() {
        blocks.push(cur);
    }

    let (header, body_start) = match blocks.first() {
        Some(first) if first[0].trim_start().starts_with("WEBVTT") => (
            first.iter().map(|l| (*l).to_string()).collect::<Vec<_>>(),
            1,
        ),
        _ => (vec!["WEBVTT".to_string()], 0),
    };

    enum Entry {
        Meta(Vec<String>),
        Cue {
            id: Option<String>,
            timing: String,
            end_secs: f64,
            lines: Vec<String>,
        },
    }

    let mut entries: Vec<Entry> = Vec::new();
    let mut last_cue: Option<usize> = None;
    let mut repaired_orphans = 0usize;

    for block in &blocks[body_start..] {
        let first = block[0].trim();
        if first == "STYLE" || first == "REGION" {
            entries.push(Entry::Meta(
                block.iter().map(|l| (*l).to_string()).collect(),
            ));
            last_cue = None;
            continue;
        }
        if first == "NOTE" || first.starts_with("NOTE ") || first.starts_with("NOTE\t") {
            last_cue = None;
            continue;
        }

        let first_timing = block
            .iter()
            .position(|l| parse_vtt_timing_line(l).is_some());
        let Some(pos) = first_timing else {
            // Orphan block with no timestamp: continuation lines separated by a
            // stray blank line inside the previous cue.
            if let Some(idx) = last_cue
                && let Entry::Cue { lines, .. } = &mut entries[idx]
            {
                lines.extend(block.iter().map(|l| (*l).to_string()));
                repaired_orphans += 1;
            }
            continue;
        };

        let mut first_id = if pos == 1 {
            Some(block[0].to_string())
        } else {
            if pos >= 2
                && let Some(idx) = last_cue
                && let Entry::Cue { lines, .. } = &mut entries[idx]
            {
                lines.extend(block[..pos].iter().map(|l| (*l).to_string()));
                repaired_orphans += 1;
            }
            None
        };

        for (offset, line) in block[pos..].iter().enumerate() {
            if let Some((timing, _start, end_secs)) = parse_vtt_timing_line(line) {
                entries.push(Entry::Cue {
                    id: if offset == 0 { first_id.take() } else { None },
                    timing,
                    end_secs,
                    lines: Vec::new(),
                });
                last_cue = Some(entries.len() - 1);
            } else if let Some(idx) = last_cue
                && let Entry::Cue { lines, .. } = &mut entries[idx]
            {
                lines.push((*line).to_string());
            }
        }
    }

    let mut out_blocks: Vec<String> = Vec::with_capacity(entries.len() + 1);
    out_blocks.push(header.join("\n"));
    let mut cue_count = 0usize;
    let mut thumb_cues = 0usize;
    let mut last_end_secs = 0.0f64;

    for entry in entries {
        match entry {
            Entry::Meta(lines) => out_blocks.push(lines.join("\n")),
            Entry::Cue {
                id,
                timing,
                end_secs,
                lines,
            } => {
                if !lines.iter().any(|l| !l.trim().is_empty()) {
                    continue;
                }
                cue_count += 1;
                if end_secs > last_end_secs {
                    last_end_secs = end_secs;
                }
                if lines.iter().all(|l| l.contains("#xywh=")) {
                    thumb_cues += 1;
                }
                let mut parts = Vec::with_capacity(lines.len() + 2);
                if let Some(id) = id {
                    parts.push(id);
                }
                parts.push(timing);
                parts.extend(lines);
                out_blocks.push(parts.join("\n"));
            }
        }
    }

    let is_thumbnail = cue_count > 0 && thumb_cues * 5 >= cue_count * 4;
    let mut content = out_blocks.join("\n\n");
    content.push('\n');
    SanitizedVtt {
        content,
        cue_count,
        last_end_secs,
        is_thumbnail,
        repaired_orphans,
    }
}

/// Counts `Dialogue:` events and finds the latest end timestamp in ASS/SSA.
fn ass_stats(text: &str) -> (usize, f64) {
    let mut count = 0usize;
    let mut last_end = 0.0f64;
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("Dialogue:") else {
            continue;
        };
        count += 1;
        if let Some(end_str) = rest.split(',').nth(2)
            && let Some(ms) = parse_vtt_timestamp_ms(end_str.trim())
        {
            last_end = last_end.max(ms as f64 / 1000.0);
        }
    }
    (count, last_end)
}

fn looks_like_html(trimmed: &str) -> bool {
    let head = trimmed.get(..16).unwrap_or(trimmed).to_ascii_lowercase();
    head.starts_with("<!doctype") || head.starts_with("<html")
}

fn url_extension(url: &str) -> &str {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .map_or("", |(_, ext)| ext)
}

struct PreparedSub {
    url: String,
    bytes: Vec<u8>,
    ext: &'static str,
    lang: LangTier,
    url_rank: i32,
    cue_count: usize,
    last_end_secs: f64,
}

impl PreparedSub {
    fn score(&self) -> (LangTier, bool, u32, usize, i32) {
        // Substantive tracks (>=20 cues) beat sparse signs-only tracks; bucket
        // last cue time to whole minutes so full-episode tracks beat truncated
        // ones while ties break by cue count and URL heuristics.
        let substantive = self.cue_count >= 20;
        let last_min = (self.last_end_secs / 60.0).max(0.0) as u32;
        (
            self.lang,
            substantive,
            last_min,
            self.cue_count,
            self.url_rank,
        )
    }
}

/// Downloads (or reads, for local paths) a subtitle candidate, enforcing a size
/// cap and passing `referrer` on HTTP requests.
fn fetch_subtitle_bytes(source: &str, referrer: Option<&str>) -> Option<Vec<u8>> {
    if source.starts_with("http://") || source.starts_with("https://") {
        let mut req = SUBTITLE_AGENT.get(source);
        if let Some(r) = referrer
            && !r.is_empty()
            && r.bytes().all(|b| (0x20..=0x7e).contains(&b))
        {
            req = req.header("Referer", r);
        }
        return req
            .call()
            .ok()?
            .body_mut()
            .with_config()
            .limit(MAX_SUBTITLE_BYTES)
            .read_to_vec()
            .ok();
    }
    let path = Path::new(source);
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_SUBTITLE_BYTES {
        return None;
    }
    fs::read(path).ok()
}

fn inspect_subtitle(source: &str, raw: Vec<u8>) -> Option<PreparedSub> {
    let text = String::from_utf8_lossy(&raw);
    let trimmed = text.strip_prefix('\u{feff}').unwrap_or(&text).trim_start();
    if trimmed.is_empty() || looks_like_html(trimmed) {
        return None;
    }
    let (lang, _, _) = sub_url_meta(source);
    let url_rank = sub_url_rank(source);
    let ext_hint = url_extension(source).to_ascii_lowercase();

    if trimmed
        .get(..13)
        .is_some_and(|h| h.eq_ignore_ascii_case("[script info]"))
    {
        let (cue_count, last_end_secs) = ass_stats(trimmed);
        if cue_count == 0 {
            return None;
        }
        return Some(PreparedSub {
            url: source.to_string(),
            bytes: raw,
            ext: "ass",
            lang,
            url_rank,
            cue_count,
            last_end_secs,
        });
    }

    if trimmed.starts_with("WEBVTT") || (ext_hint == "vtt" && trimmed.contains("-->")) {
        let sanitized = sanitize_webvtt(&text);
        if sanitized.is_thumbnail || sanitized.cue_count == 0 {
            return None;
        }
        return Some(PreparedSub {
            url: source.to_string(),
            bytes: sanitized.content.into_bytes(),
            ext: "vtt",
            lang,
            url_rank,
            cue_count: sanitized.cue_count,
            last_end_secs: sanitized.last_end_secs,
        });
    }

    if ext_hint == "srt" && trimmed.contains("-->") {
        let stats = sanitize_webvtt(&text);
        if stats.is_thumbnail || stats.cue_count == 0 {
            return None;
        }
        return Some(PreparedSub {
            url: source.to_string(),
            bytes: raw,
            ext: "srt",
            lang,
            url_rank,
            cue_count: stats.cue_count,
            last_end_secs: stats.last_end_secs,
        });
    }

    None
}

/// Writes a sanitized subtitle into `<cache_dir>/subs/sub-<hash>.<ext>` and
/// prunes older entries beyond [`MAX_CACHED_SUBS`].
fn cache_subtitle(shims: &Path, sub: &PreparedSub) -> io::Result<PathBuf> {
    let dir = shims.parent().unwrap_or(shims).join(SUBS_DIR);
    fs::create_dir_all(&dir)?;
    let digest = Sha256::digest(sub.url.as_bytes());
    let mut hex = String::with_capacity(16);
    for b in &digest[..8] {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{b:02x}");
    }
    let name = format!("sub-{hex}.{}", sub.ext);
    let path = dir.join(&name);
    let tmp = temp_name(&dir, &name);
    fs::write(&tmp, &sub.bytes)?;
    replace(&tmp, &path)?;
    prune_cached_subs(&dir, &path);
    Ok(path)
}

fn prune_cached_subs(dir: &Path, keep: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(SystemTime, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p != keep
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("sub-"))
        })
        .map(|p| {
            let mtime = fs::metadata(&p)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            (mtime, p)
        })
        .collect();
    if files.len() < MAX_CACHED_SUBS {
        return;
    }
    files.sort_by_key(|(mtime, _)| *mtime);
    let excess = files.len() + 1 - MAX_CACHED_SUBS;
    for (_, path) in files.into_iter().take(excess) {
        let _ = fs::remove_file(path);
    }
}

/// Fetches, repairs (for WebVTT) and selects the best subtitle among
/// `sub_files`, caching it beside `shims` so mpv loads a clean local file.
/// Falls back to `stream.sub_file`'s original URL on network/IO failure.
fn prepare_subtitles(
    stream: &mut Stream,
    sub_files: &[String],
    shims: &Path,
    owner: Option<&ResolveHandle>,
) {
    if sub_files.is_empty() {
        return;
    }
    let cancelled = || owner.is_some_and(|o| lock(&o.owner).cancelled);
    if cancelled() {
        return;
    }

    let mut ordered: Vec<&String> = sub_files.iter().collect();
    ordered.sort_by_key(|a| std::cmp::Reverse(sub_url_rank(a)));

    let mut candidates: Vec<&String> = ordered
        .iter()
        .copied()
        .filter(|u| !sub_url_meta(u).2)
        .take(MAX_SUB_CANDIDATES)
        .collect();
    if candidates.is_empty()
        && let Some(first) = ordered.first()
    {
        candidates.push(first);
    }

    let mut best: Option<PreparedSub> = None;
    for source in candidates {
        if cancelled() {
            return;
        }
        let Some(bytes) = fetch_subtitle_bytes(source, stream.referrer.as_deref()) else {
            continue;
        };
        let Some(prepared) = inspect_subtitle(source, bytes) else {
            continue;
        };
        if best.as_ref().is_none_or(|b| prepared.score() > b.score()) {
            best = Some(prepared);
        }
    }

    if let Some(chosen) = best
        && let Ok(path) = cache_subtitle(shims, &chosen)
    {
        stream.sub_file = Some(path.to_string_lossy().into_owned());
    }
}

/// Whoever asked for resolves (a page or the player). Dropping it, or
/// [`cancel_all_resolves`] at quit, kills the ani-cli runs it started, so
/// ani-cli doesn't record an episode nobody is going to watch.
pub struct ResolveScope {
    owner: Arc<Mutex<Owner>>,
}

/// What a resolve run is handed to register its ani-cli process group.
#[derive(Clone)]
pub struct ResolveHandle {
    owner: Arc<Mutex<Owner>>,
}

#[derive(Default)]
struct Owner {
    cancelled: bool,
    /// Process groups of its running ani-cli, removed once reaped.
    groups: Vec<Pid>,
}

/// Every live scope, for [`cancel_all_resolves`].
static SCOPES: Mutex<Vec<Weak<Mutex<Owner>>>> = Mutex::new(Vec::new());

impl Default for ResolveScope {
    fn default() -> Self {
        let owner = Arc::new(Mutex::new(Owner::default()));
        let mut scopes = lock(&SCOPES);
        scopes.retain(|w| w.strong_count() > 0);
        scopes.push(Arc::downgrade(&owner));
        ResolveScope { owner }
    }
}

impl ResolveScope {
    pub fn handle(&self) -> ResolveHandle {
        ResolveHandle {
            owner: self.owner.clone(),
        }
    }
}

impl Drop for ResolveScope {
    fn drop(&mut self) {
        cancel(&self.owner);
    }
}

fn cancel(owner: &Mutex<Owner>) {
    let mut owner = lock(owner);
    owner.cancelled = true;
    for pgid in owner.groups.drain(..) {
        let _ = killpg(pgid, Signal::SIGKILL);
    }
}

/// For app exit: kills every resolve still running.
pub fn cancel_all_resolves() {
    let owners: Vec<_> = lock(&SCOPES).iter().filter_map(Weak::upgrade).collect();
    for owner in owners {
        cancel(&owner);
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Resolves the stream for one episode by running ani-cli with the shims in
/// `shims` (see [`ensure_shims`]) as its player and menu. ani-cli records the
/// episode in its own history as it goes, unless `owner` is cancelled first.
pub fn resolve(
    req: &EpisodeRequest,
    shims: &Path,
    owner: &ResolveHandle,
) -> Result<ResolveOutcome> {
    let program = find().with_context(not_installed)?;
    resolve_with(&program, req, shims, &[], RESOLVE_TIMEOUT, Some(owner))
}

/// [`resolve`] with a given ani-cli and extra environment. When the search
/// finds nothing, it is retried once with the last (romaji) title, unless
/// the request already searches for it or picks a result by index.
pub(crate) fn resolve_with(
    program: &Path,
    req: &EpisodeRequest,
    shims: &Path,
    extra_env: &[(&str, OsString)],
    timeout: Duration,
    owner: Option<&ResolveHandle>,
) -> Result<ResolveOutcome> {
    match resolve_once(program, req, shims, extra_env, timeout, owner)? {
        Attempt::Done(outcome) => Ok(outcome),
        Attempt::NoResults(err) => {
            let romaji = req.want_titles.last().filter(|t| **t != req.query);
            match romaji {
                Some(title) if req.select_index.is_none() => {
                    let retry = EpisodeRequest {
                        query: title.clone(),
                        ..req.clone()
                    };
                    match resolve_once(program, &retry, shims, extra_env, timeout, owner)? {
                        Attempt::Done(outcome) => Ok(outcome),
                        Attempt::NoResults(err) => Err(err),
                    }
                }
                _ => Err(err),
            }
        }
    }
}

enum Attempt {
    Done(ResolveOutcome),
    NoResults(anyhow::Error),
}

fn resolve_once(
    program: &Path,
    req: &EpisodeRequest,
    shims: &Path,
    extra_env: &[(&str, OsString)],
    timeout: Duration,
    owner: Option<&ResolveHandle>,
) -> Result<Attempt> {
    let mut cmd = vec![program.as_os_str().to_owned(), "--no-detach".into()];
    cmd.extend(episode_args(req)?);
    let menu_out = menu_out_path(shims);
    let mut env = shim_env(req, shims, &menu_out);
    // ani-cli runs the player unquoted, so a full path breaks on spaces; the
    // shim dir leads PATH and the bare name still matches its `*mpv*` case.
    env.push(("ANI_CLI_PLAYER", CAPTURE_NAME.into()));
    env.extend(extra_env.iter().cloned());
    let run = run_capture(&cmd, &env, timeout, owner);
    let picks = take_menu_out(&menu_out);
    let out = run?;
    if let Some(titles) = picks {
        return Ok(Attempt::Done(ResolveOutcome::NeedsPick(titles)));
    }
    let output = format!("{}\n{}", out.stdout, out.stderr);
    if !out.status.success() {
        let err = anyhow!("ani-cli failed ({}): {}", out.status, error_detail(&output));
        if output.contains(NO_RESULTS) {
            return Ok(Attempt::NoResults(err));
        }
        return Err(err);
    }
    let (mut stream, sub_files) = parse_capture_with_subs(&out.stdout)
        .map_err(|err| anyhow!("{err}: {}", error_detail(&output)))?;
    // A lone search result is played without asking the menu, so check it.
    if req.select_index.is_none()
        && let Some(found) = stream.title.as_deref().map(|t| show_title(t, &req.ep))
    {
        let found_norm = normalize_title(found);
        if !req
            .want_titles
            .iter()
            .any(|w| normalize_title(w) == found_norm)
        {
            return Ok(Attempt::Done(ResolveOutcome::NeedsPick(vec![
                found.to_string(),
            ])));
        }
    }
    prepare_subtitles(&mut stream, &sub_files, shims, owner);
    Ok(Attempt::Done(ResolveOutcome::Stream(stream)))
}

/// The show part of ani-cli's "<title> Episode <ep>" media title.
fn show_title<'a>(media_title: &'a str, ep: &str) -> &'a str {
    media_title
        .strip_suffix(&format!(" Episode {ep}"))
        .or_else(|| media_title.rfind(" Episode ").map(|i| &media_title[..i]))
        .unwrap_or(media_title)
}

/// Environment shared by resolve and download runs: the shim dir first on
/// PATH (see [`tool_path`]), the menu shim, and what it should pick or
/// report. ani-cli settings a user may export are pinned to what the request
/// needs: there is no `--sub` flag to undo an exported `ANI_CLI_MODE=dub`.
pub(crate) fn shim_env(
    req: &EpisodeRequest,
    shims: &Path,
    menu_out: &Path,
) -> Vec<(&'static str, OsString)> {
    let mut path = OsString::from(shims.as_os_str());
    path.push(":");
    path.push(tool_path());
    vec![
        ("PATH", path),
        ("ANI_CLI_MENU", shims.join(MENU_NAME).into_os_string()),
        ("ANI_CLI_MENU_FLAGS", "".into()),
        ("ANI_CLI_MODE", req.mode.as_str().into()),
        ("ANI_CLI_DEFAULT_SOURCE", "search".into()),
        ("ANI_CLI_EXACT", "0".into()),
        ("ANI_CLI_PLAYER_FLAGS", "".into()),
        ("ANI_CLI_SKIP_INTRO", "0".into()),
        ("ANI_GUI_WANT_TITLES", req.want_titles.join("\n").into()),
        ("ANI_GUI_MENU_OUT", menu_out.as_os_str().to_owned()),
    ]
}

/// The PATH ani-cli finds its tools on: [`EXTRA_BIN_DIRS`] and
/// `~/.local/bin` (for sessions started from a desktop launcher), then the
/// app's own PATH.
pub fn tool_path() -> OsString {
    let current = env::var_os("PATH").unwrap_or_default();
    let dirs = EXTRA_BIN_DIRS
        .iter()
        .map(PathBuf::from)
        .chain(env::home_dir().map(|home| home.join(".local/bin")))
        .chain(env::split_paths(&current));
    env::join_paths(dirs).unwrap_or(current)
}

/// A fresh path for one run's menu-out file.
pub(crate) fn menu_out_path(shims: &Path) -> PathBuf {
    temp_name(shims, "menu-out")
}

/// The candidate titles the menu shim wrote, if any; removes the file.
pub(crate) fn take_menu_out(path: &Path) -> Option<Vec<String>> {
    let json = fs::read_to_string(path).ok();
    let _ = fs::remove_file(path);
    let titles: Vec<String> = serde_json::from_str(&json?).ok()?;
    (!titles.is_empty()).then_some(titles)
}

/// `[-S <i>] -e <ep> -q <quality> [--dub] <query>`; an error unless the
/// episode is a plain number (see [`valid_episode`]).
pub(crate) fn episode_args(req: &EpisodeRequest) -> Result<Vec<OsString>> {
    check_episode(&req.ep)?;
    let quality = if req.quality.is_empty() {
        "best"
    } else {
        &req.quality
    };
    let mut args: Vec<OsString> = Vec::new();
    if let Some(index) = req.select_index {
        args.extend(["-S".into(), index.to_string().into()]);
    }
    args.extend([
        "-e".into(),
        req.ep.as_str().into(),
        "-q".into(),
        quality.into(),
    ]);
    if req.mode == Mode::Dub {
        args.push("--dub".into());
    }
    args.push(search_query(&req.query).into());
    Ok(args)
}

/// Refuses an episode ani-cli must never see (see [`valid_episode`]).
pub(crate) fn check_episode(ep: &str) -> Result<()> {
    if !valid_episode(ep) {
        bail!("{ep:?} is not a valid episode number");
    }
    Ok(())
}

/// `query` as ani-cli must receive it: ani-cli only turns spaces into `+`
/// before pasting it into hianime's `search?keyword=` URL, so everything
/// else that isn't URL-safe is percent-encoded here (hianime answers
/// HTTP 400 for raw non-ASCII such as AllAnime's "Journey’s").
fn search_query(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    for (i, b) in query.replace(['’', '‘'], "'").bytes().enumerate() {
        // A leading `-` would make ani-cli parse the query as an option
        // (`-D` wipes its history), so it is encoded like any other byte.
        let keep = b == b' ' || b.is_ascii_alphanumeric() || b"-._~".contains(&b);
        if keep && !(i == 0 && b == b'-') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The tail of ani-cli's output, without colour codes, blank lines and
/// `tput` complaints, for showing in an error.
pub(crate) fn error_detail(output: &str) -> String {
    let text = strip_ansi(output)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("tput:"))
        .collect::<Vec<_>>()
        .join("\n");
    match text.char_indices().rev().nth(DETAIL_CHARS - 1) {
        Some((start, _)) => text[start..].to_string(),
        None => text,
    }
}

/// Removes terminal escape sequences (CSI, OSC, two-byte) and carriage returns.
pub(crate) fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {}
            '\x1b' => match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            c => out.push(c),
        }
    }
    out
}

struct Captured {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

/// Runs `cmd` in its own process group with stdin from /dev/null, collecting
/// stdout and stderr. Past `timeout` the whole group is killed and the error
/// carries the output so far. While it runs, cancelling `owner` kills it.
fn run_capture(
    cmd: &[OsString],
    env: &[(&str, OsString)],
    timeout: Duration,
    owner: Option<&ResolveHandle>,
) -> Result<Captured> {
    let cancelled = || owner.is_some_and(|o| lock(&o.owner).cancelled);
    if cancelled() {
        bail!("cancelled");
    }
    let (program, args) = cmd.split_first().context("empty command")?;
    let mut command = Command::new(program);
    command
        .args(args)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child =
        spawn(&mut command).with_context(|| format!("starting {}", program.to_string_lossy()))?;
    let stdout = OutputReader::spawn(child.stdout.take());
    let stderr = OutputReader::spawn(child.stderr.take());
    // process_group(0) made the child the leader of its own group.
    let pgid = Pid::from_raw(i32::try_from(child.id()).unwrap_or(i32::MAX));
    if let Some(owner) = owner {
        let mut owner = lock(&owner.owner);
        if owner.cancelled {
            let _ = killpg(pgid, Signal::SIGKILL);
        } else {
            owner.groups.push(pgid);
        }
    }
    // Reaps under the owner's lock and forgets the group, so a cancel never
    // signals a group id that may have been reused.
    let forget = |child: &mut Child, kill: bool| -> io::Result<Option<ExitStatus>> {
        let mut owner = owner.map(|o| lock(&o.owner));
        if kill {
            // Killed before reaping, so the group id cannot have been reused.
            let _ = killpg(pgid, Signal::SIGKILL);
        }
        let status = if kill {
            child.wait().map(Some)
        } else {
            child.try_wait()
        };
        if !matches!(status, Ok(None))
            && let Some(owner) = owner.as_mut()
        {
            owner.groups.retain(|g| *g != pgid);
        }
        status
    };

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = forget(&mut child, false)? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = forget(&mut child, true);
            break None;
        }
        thread::sleep(Duration::from_millis(25));
    };
    let stdout = stdout.finish();
    let stderr = stderr.finish();
    if cancelled() {
        bail!("cancelled");
    }
    match status {
        Some(status) => Ok(Captured {
            status,
            stdout,
            stderr,
        }),
        None => bail!(
            "ani-cli timed out after {} s: {}",
            timeout.as_secs(),
            error_detail(&format!("{stdout}\n{stderr}"))
        ),
    }
}

/// `cmd.spawn()`, retried briefly on ETXTBSY: a script that was just written
/// stays "busy" while any thread's fork still holds the writer's fd.
pub(crate) fn spawn(cmd: &mut Command) -> io::Result<Child> {
    let mut tries = 0;
    loop {
        match cmd.spawn() {
            Err(err) if err.kind() == io::ErrorKind::ExecutableFileBusy && tries < 10 => {
                tries += 1;
                thread::sleep(Duration::from_millis(20));
            }
            result => return result,
        }
    }
}

/// Drains a pipe on a thread so neither stream can block the child.
struct OutputReader {
    buf: Arc<Mutex<Vec<u8>>>,
    eof: mpsc::Receiver<()>,
}

impl OutputReader {
    fn spawn(pipe: Option<impl Read + Send + 'static>) -> OutputReader {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let (tx, eof) = mpsc::channel();
        if let Some(mut pipe) = pipe {
            let buf = buf.clone();
            thread::spawn(move || {
                let mut chunk = [0u8; 4096];
                loop {
                    match pipe.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => buf
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .extend_from_slice(&chunk[..n]),
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
                let _ = tx.send(());
            });
        }
        OutputReader { buf, eof }
    }

    /// Output so far, waiting briefly for EOF in case a straggling process
    /// still holds the pipe.
    fn finish(self) -> String {
        let _ = self.eof.recv_timeout(Duration::from_secs(2));
        let buf = self.buf.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&buf).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    use super::*;

    const SAMPLE: &str = "\x1b[2K\x1b[1;34mChecking dependencies...\x1b[0m\n\
        tput: No value for $TERM and no -T specified\n\
        \x1b[2K\x1b[1;34mhianime.at links fetched\x1b[0m\n\
        --referrer=https://zokoanime.video/\n\
        --sub-file=https://hls.dramahot.top/v/2f7c1e9a4b3d8e6f0a1b2c3d4e5f6a7b/subs/eng-2.vtt\n\
        --force-media-title=Frieren: Beyond Journey's End Season 2 Episode 1\n\
        https://hls.dramahot.top/p/9d8c7b6a5f4e3d2c1b0a99887766554433221100/eyJ0IjoxNzk1NDM3MDAwfQ/master.m3u8\n";

    #[test]
    fn parse_capture_full() {
        let stream = parse_capture(SAMPLE).unwrap();
        assert_eq!(
            stream,
            Stream {
                url: "https://hls.dramahot.top/p/9d8c7b6a5f4e3d2c1b0a99887766554433221100/eyJ0IjoxNzk1NDM3MDAwfQ/master.m3u8".into(),
                referrer: Some("https://zokoanime.video/".into()),
                audio_file: None,
                sub_file: Some(
                    "https://hls.dramahot.top/v/2f7c1e9a4b3d8e6f0a1b2c3d4e5f6a7b/subs/eng-2.vtt"
                        .into()
                ),
                title: Some("Frieren: Beyond Journey's End Season 2 Episode 1".into()),
            }
        );

        let with_audio = parse_capture(
            "--referrer=https://r.example/\r\n--audio-file=https://a.example/en.m3u8\r\n\
             --force-media-title=T Episode 2\r\nhttps://v.example/first.m3u8\n\
             \x1b[1;32mhttps://v.example/last.m3u8\x1b[0m\n",
        )
        .unwrap();
        assert_eq!(with_audio.url, "https://v.example/last.m3u8");
        assert_eq!(
            with_audio.audio_file.as_deref(),
            Some("https://a.example/en.m3u8")
        );
        assert_eq!(with_audio.sub_file, None);
        assert_eq!(with_audio.title.as_deref(), Some("T Episode 2"));
    }

    #[test]
    fn parse_capture_rejects_missing_url() {
        let no_url = "\x1b[2K\x1b[1;34mChecking dependencies...\x1b[0m\n\
            --referrer=https://zokoanime.video/\n\
            --force-media-title=Frieren Episode 1\n";
        let err = parse_capture(no_url).unwrap_err().to_string();
        assert!(err.contains("no stream URL"), "{err}");
        assert!(parse_capture("").is_err());
    }

    #[test]
    fn resolve_times_out() {
        let cmd: Vec<OsString> = ["sh", "-c", "echo started; sleep 5"]
            .map(OsString::from)
            .to_vec();
        let begun = Instant::now();
        let err = run_capture(&cmd, &[], Duration::from_secs(1), None)
            .err()
            .expect("sleeping script must time out")
            .to_string();
        assert!(
            begun.elapsed() < Duration::from_secs(4),
            "{:?}",
            begun.elapsed()
        );
        assert!(err.contains("timed out"), "{err}");
        assert!(err.contains("started"), "partial output in error: {err}");

        let ok = run_capture(
            &["sh", "-c", "echo \"$X\"; echo oops >&2; exit 3"].map(OsString::from),
            &[("X", "from-env".into())],
            Duration::from_secs(5),
            None,
        )
        .unwrap();
        assert_eq!(ok.stdout, "from-env\n");
        assert_eq!(ok.stderr, "oops\n");
        assert_eq!(ok.status.code(), Some(3));
    }

    #[test]
    fn dropping_scope_kills_running_resolve() {
        let dir = temp_dir("cancel");
        let shims = ensure_shims(&dir, &std::env::current_exe().unwrap()).unwrap();
        let pid_file = dir.join("child.pid");
        let ran = dir.join("ran");
        let fake = script(
            &dir.join("bin/ani-cli"),
            &format!(
                "touch '{}'\nsleep 30 &\necho $! > '{}'\nwait",
                ran.display(),
                pid_file.display()
            ),
        );
        let req = build_request(&frieren(), None, "1", Mode::Sub, "best");

        let scope = ResolveScope::default();
        let handle = scope.handle();
        let (fake2, req2, shims2) = (fake.clone(), req.clone(), shims.clone());
        let begun = Instant::now();
        let run = thread::spawn(move || {
            resolve_with(
                &fake2,
                &req2,
                &shims2,
                &[],
                Duration::from_secs(20),
                Some(&handle),
            )
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while fs::read_to_string(&pid_file).map_or(true, |p| p.trim().is_empty()) {
            assert!(Instant::now() < deadline, "fake ani-cli never started");
            thread::sleep(Duration::from_millis(20));
        }
        drop(scope);
        let err = run.join().unwrap().unwrap_err().to_string();
        assert!(err.contains("cancelled"), "{err}");
        assert!(
            begun.elapsed() < Duration::from_secs(5),
            "{:?}",
            begun.elapsed()
        );
        let child: i32 = fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while nix::sys::signal::kill(Pid::from_raw(child), None).is_ok() {
            assert!(Instant::now() < deadline, "grandchild {child} survived");
            thread::sleep(Duration::from_millis(20));
        }

        // A cancelled scope never starts ani-cli again.
        fs::remove_file(&ran).unwrap();
        let scope = ResolveScope::default();
        let handle = scope.handle();
        cancel_all_resolves();
        let err = resolve_with(
            &fake,
            &req,
            &shims,
            &[],
            Duration::from_secs(5),
            Some(&handle),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("cancelled"), "{err}");
        assert!(!ran.exists(), "ani-cli ran for a cancelled scope");
        drop(scope);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn error_detail_strips_ansi_and_keeps_tail() {
        let out = "\x1b[2K\x1b[1;34mChecking dependencies...\x1b[0m\r\n\
            tput: No value for $TERM and no -T specified\n\
            \x1b]0;title\x07\x1b7\n\
            \x1b[1;31mEpisode not released!\x1b[0m\n";
        assert_eq!(
            error_detail(out),
            "Checking dependencies...\nEpisode not released!"
        );
        let long = format!("{}ü end", "x".repeat(300));
        let tail = error_detail(&long);
        assert_eq!(tail.chars().count(), 240);
        assert!(tail.ends_with("xü end"));
    }

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(name: &str) -> PathBuf {
        let n = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("ani-gui-anicli-{name}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn script(path: &Path, body: &str) -> PathBuf {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        path.to_path_buf()
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn frieren() -> ShowSummary {
        ShowSummary {
            id: "ReHMC7TQnch3C6z8j".into(),
            title: "Sousou no Frieren".into(),
            english_title: Some("Frieren: Beyond Journey’s End".into()),
            sub_count: 28,
            dub_count: 28,
            ..Default::default()
        }
    }

    #[test]
    fn ensure_shims_writes_scripts_and_links() {
        let dir = temp_dir("shims");
        // A path that needs shell quoting.
        let exe = script(
            &dir.join("it's a \"dir\" $HOME").join("fake ani-gui"),
            r#"printf '%s\n' "$@""#,
        );
        let shims = ensure_shims(&dir.join("cache"), &exe).unwrap();
        assert_eq!(shims, dir.join("cache").join("shims"));
        // Rewriting existing shims is fine.
        ensure_shims(&dir.join("cache"), &exe).unwrap();

        let run = |name: &str, args: &[&str]| {
            let out = Command::new(shims.join(name)).args(args).output().unwrap();
            String::from_utf8(out.stdout).unwrap()
        };
        assert_eq!(
            run(
                "capture-mpv",
                &["--referrer=https://r/", "a b", "https://v/x.m3u8"]
            ),
            "--referrer=https://r/\na b\nhttps://v/x.m3u8\n"
        );
        for menu in ["menu", "rofi", "dmenu"] {
            assert_eq!(
                run(menu, &["-p", "Select anime: "]),
                "--ani-cli-menu\n-p\nSelect anime: \n",
                "{menu}"
            );
        }
        assert_eq!(
            fs::read_link(shims.join("rofi")).unwrap(),
            PathBuf::from("menu")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn episode_args_search_with_ascii_apostrophes() {
        let req = build_request(&frieren(), None, "1", Mode::Sub, "best");
        assert_eq!(
            episode_args(&req).unwrap(),
            ["-e", "1", "-q", "best", "Frieren%3A Beyond Journey%27s End"]
        );
    }

    #[test]
    fn not_installed_names_every_folder_searched() {
        let expected = if cfg!(target_os = "macos") {
            "ani-cli is not installed (looked on PATH, in ~/.local/bin, \
             /opt/homebrew/bin and /usr/local/bin)"
        } else {
            "ani-cli is not installed (looked on PATH, in ~/.local/bin and /usr/local/bin)"
        };
        assert_eq!(not_installed(), expected);
    }

    #[test]
    fn shim_env_pins_ani_cli_settings() {
        let dir = Path::new("/tmp/shims");
        let value = |env: &[(&str, OsString)], key: &str| {
            let found: Vec<_> = env.iter().filter(|(k, _)| *k == key).collect();
            assert_eq!(found.len(), 1, "{key}");
            found[0].1.to_string_lossy().into_owned()
        };
        for (mode, expect) in [(Mode::Sub, "sub"), (Mode::Dub, "dub")] {
            let req = build_request(&frieren(), None, "1", mode, "best");
            let env = shim_env(&req, dir, &dir.join("out"));
            // Set explicitly, so they override whatever the user exported.
            assert_eq!(value(&env, "ANI_CLI_MODE"), expect);
            assert_eq!(value(&env, "ANI_CLI_DEFAULT_SOURCE"), "search");
            assert_eq!(value(&env, "ANI_CLI_EXACT"), "0");
            assert_eq!(value(&env, "ANI_CLI_PLAYER_FLAGS"), "");
            assert_eq!(value(&env, "ANI_CLI_SKIP_INTRO"), "0");
            assert_eq!(value(&env, "ANI_CLI_MENU_FLAGS"), "");
            let path = value(&env, "PATH");
            let first = format!("/tmp/shims:{}:", EXTRA_BIN_DIRS[0]);
            assert!(path.starts_with(&first), "{path}");
            assert!(path.ends_with(&*tool_path().to_string_lossy()), "{path}");
        }
    }

    #[test]
    fn episode_args_reject_non_numeric_episode() {
        let mut req = build_request(&frieren(), None, "1", Mode::Sub, "best");
        req.ep = "1\\|x$|touch FILE|e;#".into();
        let err = episode_args(&req).unwrap_err().to_string();
        assert!(err.contains("not a valid episode"), "{err}");
        req.ep = "2.5".into();
        assert!(episode_args(&req).is_ok());
    }

    #[test]
    fn resolve_rejects_bad_episode_without_running_ani_cli() {
        let dir = temp_dir("bad-ep");
        let shims = ensure_shims(&dir, &std::env::current_exe().unwrap()).unwrap();
        let ran = dir.join("ran");
        let fake = script(
            &dir.join("bin/ani-cli"),
            &format!("touch '{}'", ran.display()),
        );
        let mut req = build_request(&frieren(), None, "1", Mode::Sub, "best");
        req.ep = "1\\|x$|touch FILE|e;#".into();
        assert!(resolve_with(&fake, &req, &shims, &[], Duration::from_secs(10), None).is_err());
        assert!(!ran.exists(), "ani-cli ran for an invalid episode");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn search_query_percent_encodes_all_but_spaces() {
        assert_eq!(
            search_query("Re:Zero − Starting Life in Another World"),
            "Re%3AZero %E2%88%92 Starting Life in Another World"
        );
        assert_eq!(search_query("Tom & Jerry?"), "Tom %26 Jerry%3F");
        assert_eq!(search_query("100% #1 a+b/c"), "100%25 %231 a%2Bb%2Fc");
        assert_eq!(search_query("Kaguya‘s ’Love’"), "Kaguya%27s %27Love%27");
        assert_eq!(search_query("Dr._Stone-2~x"), "Dr._Stone-2~x");
    }

    #[test]
    fn search_query_never_starts_with_a_dash() {
        assert_eq!(search_query("-D"), "%2DD");
        assert_eq!(search_query("--help"), "%2D-help");
        assert_eq!(search_query("Re-Kan"), "Re-Kan");
    }

    #[test]
    fn build_request_prefers_provider_title() {
        let req = build_request(
            &frieren(),
            Some("Frieren: Beyond Journey's End"),
            "3",
            Mode::Dub,
            "720",
        );
        assert_eq!(
            req,
            EpisodeRequest {
                query: "Frieren: Beyond Journey's End".into(),
                want_titles: strings(&[
                    "Frieren: Beyond Journey's End",
                    "Frieren: Beyond Journey’s End",
                    "Sousou no Frieren",
                ]),
                select_index: None,
                ep: "3".into(),
                mode: Mode::Dub,
                quality: "720".into(),
            }
        );

        let req = build_request(&frieren(), None, "1", Mode::Sub, "best");
        assert_eq!(req.query, "Frieren: Beyond Journey’s End");
        assert_eq!(
            req.want_titles,
            strings(&["Frieren: Beyond Journey’s End", "Sousou no Frieren"])
        );

        let romaji_only = ShowSummary {
            english_title: None,
            ..frieren()
        };
        let req = build_request(
            &romaji_only,
            Some("Sousou no Frieren"),
            "1",
            Mode::Sub,
            "best",
        );
        assert_eq!(req.query, "Sousou no Frieren");
        assert_eq!(req.want_titles, strings(&["Sousou no Frieren"]));
    }

    #[test]
    fn normalize_title_ignores_case_quotes_and_punctuation() {
        assert_eq!(
            normalize_title("  Frieren: Beyond   Journey’s End! "),
            "frieren beyond journeys end"
        );
        assert_eq!(
            normalize_title("Frieren: Beyond Journey's End"),
            normalize_title("FRIEREN - beyond journey’s end")
        );
    }

    const MENU_STDIN: &str =
        "1 Frieren: Beyond Journey's End\n2 Frieren: Beyond Journey's End Season 2\n";

    #[test]
    fn menu_picks_exact_normalized_title() {
        let dir = temp_dir("menu-pick");
        let out = dir.join("out.json");
        let want = strings(&["Frieren: Beyond Journey’s End", "Sousou no Frieren"]);
        // As rofi: `rofi -sort -dmenu -i -p "<prompt>"`.
        let args = strings(&["-sort", "-dmenu", "-i", "-p", "Select anime: "]);
        assert_eq!(
            menu_main(&args, MENU_STDIN, &want, Some(&out)),
            "1 Frieren: Beyond Journey's End\n"
        );
        // A custom menu program gets the prompt last.
        let want = strings(&["frieren beyond journeys end season 2"]);
        assert_eq!(
            menu_main(&strings(&["Select anime: "]), MENU_STDIN, &want, Some(&out)),
            "2 Frieren: Beyond Journey's End Season 2\n"
        );
        assert!(!out.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn menu_no_match_writes_candidates() {
        let dir = temp_dir("menu-none");
        let out = dir.join("out.json");
        let printed = menu_main(
            &strings(&["-p", "Select anime: "]),
            MENU_STDIN,
            &strings(&["Sousou no Frieren"]),
            Some(&out),
        );
        assert_eq!(printed, "");
        let written: Vec<String> =
            serde_json::from_str(&fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(
            written,
            [
                "Frieren: Beyond Journey's End",
                "Frieren: Beyond Journey's End Season 2"
            ]
        );
        assert_eq!(
            menu_main(&strings(&["Select anime: "]), MENU_STDIN, &[], None),
            ""
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn menu_postplay_prompt_prints_nothing() {
        let dir = temp_dir("menu-post");
        let out = dir.join("out.json");
        let printed = menu_main(
            &strings(&[
                "-p",
                "Playing episode 3 of Frieren: Beyond Journey's End... ",
            ]),
            "next\nreplay\nprevious\nselect\nchange_quality\nquit\n",
            &strings(&["Frieren: Beyond Journey's End"]),
            Some(&out),
        );
        assert_eq!(printed, "");
        assert!(!out.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_needs_pick_when_menu_out_written() {
        let dir = temp_dir("needs-pick");
        let shims = ensure_shims(&dir, &std::env::current_exe().unwrap()).unwrap();
        let fake = script(
            &dir.join("bin/ani-cli"),
            r#"printf '["Frieren: Beyond Journey'"'"'s End","Frieren: Beyond Journey'"'"'s End Season 2"]' > "$ANI_GUI_MENU_OUT"
printf '\033[1;31mInvalid anime selection\033[0m\n' >&2
exit 1"#,
        );
        let req = build_request(&frieren(), None, "1", Mode::Sub, "best");
        let outcome =
            resolve_with(&fake, &req, &shims, &[], Duration::from_secs(10), None).unwrap();
        assert_eq!(
            outcome,
            ResolveOutcome::NeedsPick(strings(&[
                "Frieren: Beyond Journey's End",
                "Frieren: Beyond Journey's End Season 2"
            ]))
        );
        let leftovers: Vec<_> = fs::read_dir(&shims)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.contains("menu-out"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");

        let failing = script(
            &dir.join("bin/failing"),
            r#"printf '\033[1;31mEpisode not released!\033[0m\n' >&2; exit 1"#,
        );
        let err = resolve_with(&failing, &req, &shims, &[], Duration::from_secs(10), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Episode not released!"), "{err}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_retries_romaji_on_no_results() {
        let dir = temp_dir("romaji");
        let shims = ensure_shims(&dir, &std::env::current_exe().unwrap()).unwrap();
        let log = dir.join("calls.log");
        let fake = script(
            &dir.join("bin/ani-cli"),
            &format!(
                r#"log='{}'
printf '%s|' "$@" >> "$log"
printf '%s|%s|%s|%s|%s\n' "$ANI_CLI_PLAYER" "$ANI_CLI_MENU" "$ANI_GUI_WANT_TITLES" "${{PATH%%:*}}" "$ANI_CLI_MODE" >> "$log"
for a; do last=$a; done
if [ "$last" != "Sousou no Frieren" ]; then
  printf '\033[1;31mNo results found!\033[0m\n' >&2
  exit 1
fi
echo '--referrer=https://zokoanime.video/'
echo '--force-media-title=Frieren: Beyond Journey'"'"'s End Episode 1'
echo 'https://hls.dramahot.top/p/0a1b2c3d4e5f/master.m3u8'"#,
                log.display()
            ),
        );
        let req = build_request(&frieren(), None, "1", Mode::Dub, "best");
        let outcome =
            resolve_with(&fake, &req, &shims, &[], Duration::from_secs(10), None).unwrap();
        let ResolveOutcome::Stream(stream) = outcome else {
            panic!("expected a stream, got {outcome:?}");
        };
        assert_eq!(
            stream.url,
            "https://hls.dramahot.top/p/0a1b2c3d4e5f/master.m3u8"
        );
        assert_eq!(stream.referrer.as_deref(), Some("https://zokoanime.video/"));

        let env_line = format!(
            "capture-mpv|{}|Frieren: Beyond Journey’s End\nSousou no Frieren|{}|dub",
            shims.join("menu").display(),
            shims.display()
        );
        assert_eq!(
            fs::read_to_string(&log).unwrap(),
            format!(
                "--no-detach|-e|1|-q|best|--dub|Frieren%3A Beyond Journey%27s End|{env_line}\n\
                 --no-detach|-e|1|-q|best|--dub|Sousou no Frieren|{env_line}\n"
            )
        );

        // Already searching by romaji (or by a picked index): no second run.
        fs::remove_file(&log).unwrap();
        let romaji = ShowSummary {
            english_title: None,
            title: "Shingeki".into(),
            ..frieren()
        };
        let req = build_request(&romaji, None, "1", Mode::Sub, "best");
        let err = resolve_with(&fake, &req, &shims, &[], Duration::from_secs(10), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("No results found"), "{err}");
        let picked = EpisodeRequest {
            select_index: Some(2),
            ..build_request(&frieren(), None, "1", Mode::Sub, "best")
        };
        assert!(resolve_with(&fake, &picked, &shims, &[], Duration::from_secs(10), None).is_err());
        assert_eq!(
            fs::read_to_string(&log)
                .unwrap()
                .matches("--no-detach")
                .count(),
            2
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A fake ani-cli that, like the real one, runs `$ANI_CLI_PLAYER` unquoted
    /// with the stream's arguments.
    fn fake_player_run(path: &Path, title: &str) -> PathBuf {
        script(
            path,
            &format!(
                r#"$ANI_CLI_PLAYER --referrer="https://zokoanime.video/" --force-media-title="{title} Episode 1" "https://hls.dramahot.top/p/0a1b2c3d4e5f/master.m3u8""#
            ),
        )
    }

    #[test]
    fn resolve_works_with_space_in_cache_dir() {
        let dir = temp_dir("space");
        let shims = ensure_shims(
            &dir.join("cache dir with spaces"),
            &std::env::current_exe().unwrap(),
        )
        .unwrap();
        let fake = fake_player_run(&dir.join("bin/ani-cli"), "Frieren: Beyond Journey's End");
        let req = build_request(&frieren(), None, "1", Mode::Sub, "best");
        let outcome =
            resolve_with(&fake, &req, &shims, &[], Duration::from_secs(10), None).unwrap();
        let ResolveOutcome::Stream(stream) = outcome else {
            panic!("expected a stream, got {outcome:?}");
        };
        assert_eq!(
            stream.url,
            "https://hls.dramahot.top/p/0a1b2c3d4e5f/master.m3u8"
        );
        assert_eq!(
            stream.title.as_deref(),
            Some("Frieren: Beyond Journey's End Episode 1")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_needs_pick_when_single_result_mismatches() {
        // With one search result ani-cli plays it without asking the menu.
        let dir = temp_dir("single");
        let shims = ensure_shims(&dir, &std::env::current_exe().unwrap()).unwrap();
        let fake = fake_player_run(
            &dir.join("bin/ani-cli"),
            "Frieren: Beyond Journey's End Season 2",
        );
        let req = build_request(&frieren(), None, "1", Mode::Sub, "best");
        assert_eq!(
            resolve_with(&fake, &req, &shims, &[], Duration::from_secs(10), None).unwrap(),
            ResolveOutcome::NeedsPick(strings(&["Frieren: Beyond Journey's End Season 2"]))
        );

        // A result the user picked by index is trusted.
        let picked = EpisodeRequest {
            select_index: Some(1),
            ..req
        };
        assert!(matches!(
            resolve_with(&fake, &picked, &shims, &[], Duration::from_secs(10), None).unwrap(),
            ResolveOutcome::Stream(_)
        ));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[ignore = "runs the installed ani-cli against live providers; needs `cargo build` first"]
    fn live_resolve_frieren() {
        // The menu shim runs the real binary, not this test harness.
        let exe = std::env::current_exe()
            .unwrap()
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .join("ani-gui");
        assert!(exe.exists(), "run `cargo build` first: {}", exe.display());
        let dir = temp_dir("live");
        let shims = ensure_shims(&dir.join("cache"), &exe).unwrap();
        let program = find().expect("ani-cli installed");
        let show = ShowSummary {
            title: "Sousou no Frieren".into(),
            english_title: Some("Frieren: Beyond Journey's End".into()),
            ..frieren()
        };
        let req = build_request(&show, None, "1", Mode::Sub, "best");
        let hist = dir.join("hist");
        let outcome = resolve_with(
            &program,
            &req,
            &shims,
            &[("ANI_CLI_HIST_DIR", hist.clone().into())],
            RESOLVE_TIMEOUT,
            None,
        )
        .unwrap();
        let ResolveOutcome::Stream(stream) = outcome else {
            panic!("expected a stream, got {outcome:?}");
        };
        assert!(stream.url.starts_with("http"), "{stream:?}");
        assert!(stream.referrer.is_some(), "{stream:?}");
        let entries = crate::history::read(&hist.join("ani-hsts"));
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].ep, "1");
        assert_eq!(entries[0].title, "Frieren: Beyond Journey's End");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parse_capture_prefers_english_full_over_multiple_sub_files() {
        let out = "\
            --referrer=https://zokoanime.video/\n\
            --sub-file=https://cdn.example/subs/spanish.vtt\n\
            --sub-file=https://cdn.example/subs/eng-signs.vtt\n\
            --sub-file=https://cdn.example/subs/eng-2.vtt\n\
            --sub-file=https://cdn.example/subs/thumbnails.vtt\n\
            https://cdn.example/master.m3u8\n";
        let (stream, subs) = parse_capture_with_subs(out).unwrap();
        assert_eq!(subs.len(), 4);
        assert_eq!(
            stream.sub_file.as_deref(),
            Some("https://cdn.example/subs/eng-2.vtt")
        );

        // Filename language wins over an `/en/` directory prefix.
        let path_trap = "\
            --sub-file=https://cdn.example/en/subs/spanish.vtt\n\
            --sub-file=https://cdn.example/en/subs/english.vtt\n\
            --sub-file=https://cdn.example/en/subs/spanish.vtt\n\
            https://cdn.example/master.m3u8\n";
        let (stream, subs) = parse_capture_with_subs(path_trap).unwrap();
        assert_eq!(subs.len(), 2, "duplicate sub URLs are deduplicated");
        assert_eq!(
            stream.sub_file.as_deref(),
            Some("https://cdn.example/en/subs/english.vtt")
        );

        // Opaque hashes preserve order rather than picking the last one.
        let opaque = "\
            --sub-file=https://cdn.example/subs/aaa111.vtt\n\
            --sub-file=https://cdn.example/subs/bbb222.vtt\n\
            https://cdn.example/master.m3u8\n";
        assert_eq!(
            parse_capture(opaque).unwrap().sub_file.as_deref(),
            Some("https://cdn.example/subs/aaa111.vtt")
        );
    }

    #[test]
    fn sanitize_webvtt_repairs_blank_lines_inside_cues() {
        // Makeine ep 9 pattern: blank line inside a multi-line bold sign that
        // makes FFmpeg's webvtt demuxer stop parsing at 18:09.
        let ep9_like = "\u{feff}WEBVTT\r\n\r\n\
            16:41.400 --> 16:44.860\r\n\
            Before sign\r\n\r\n\
            18:09.650 --> 18:12.610\r\n\
            <b>Edible\r\n\r\n\
            Readings</b>\r\n\r\n\
            18:20.750 --> 18:22.370\r\n\
            Yup. Perfect.\r\n\r\n\
            22:26.280 --> 22:30.030\r\n\
            Episode 9\r\n";
        let fixed = sanitize_webvtt(ep9_like);
        assert_eq!(fixed.cue_count, 4);
        assert_eq!(fixed.repaired_orphans, 1);
        assert!((fixed.last_end_secs - 1350.03).abs() < 0.01);
        assert!(!fixed.is_thumbnail);
        assert!(
            fixed
                .content
                .contains("18:09.650 --> 18:12.610\n<b>Edible\nReadings</b>\n\n18:20.750"),
            "{}",
            fixed.content
        );

        // Makeine ep 12 pattern: two consecutive orphan paragraphs inside one cue.
        let ep12_like = "WEBVTT\n\n\
            10:52.960 --> 11:00.470\n<b>Ediacaran Fauna\n\n\
            Paragraph two.\n\n\
            Paragraph three.</b>\n\n\
            11:05.000 --> 11:08.000\nDialogue resumes.\n";
        let fixed = sanitize_webvtt(ep12_like);
        assert_eq!(fixed.cue_count, 2);
        assert_eq!(fixed.repaired_orphans, 2);
        assert!(
            fixed
                .content
                .contains("<b>Ediacaran Fauna\nParagraph two.\nParagraph three.</b>"),
            "{}",
            fixed.content
        );
    }

    #[test]
    fn sanitize_webvtt_handles_edge_cases_and_thumbnails() {
        let messy = "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00:00:00.000\n\n\
            STYLE\n::cue { color: white; }\n\n\
            NOTE ignored comment\n\n\
            stray preamble\n\n\
            1\n00:01,5 --> 00:02,55 align:center\nComma decimals\n\n\
            00:03.000 --> 00:04.000\n\n\
            After blank line\n\n\
            00:05.000 --> 00:06.000\n\n\
            65:10.000 --> 65:09.000\nInverted clamped\n\
            65:12.000 --> 65:15.000\nMissing blank between cues\n";
        let fixed = sanitize_webvtt(messy);
        assert_eq!(fixed.cue_count, 4);
        assert!((fixed.last_end_secs - 3915.0).abs() < 0.01);
        assert!(fixed.content.contains("X-TIMESTAMP-MAP=MPEGTS:900000"));
        assert!(fixed.content.contains("STYLE\n::cue { color: white; }"));
        assert!(!fixed.content.contains("ignored comment"));
        assert!(!fixed.content.contains("stray preamble"));
        assert!(
            fixed
                .content
                .contains("1\n00:01.500 --> 00:02.550 align:center\nComma decimals")
        );
        assert!(
            fixed
                .content
                .contains("00:03.000 --> 00:04.000\nAfter blank line")
        );
        assert!(
            !fixed.content.contains("00:05.000 --> 00:06.000"),
            "empty cue dropped"
        );
        assert!(
            fixed
                .content
                .contains("01:05:10.000 --> 01:05:10.000\nInverted clamped")
        );
        assert!(
            fixed
                .content
                .contains("01:05:12.000 --> 01:05:15.000\nMissing blank between cues")
        );

        let sprites = "WEBVTT\n\n\
            00:00.000 --> 00:05.000\nsprite.jpg#xywh=0,0,160,90\n\n\
            00:05.000 --> 00:10.000\nsprite.jpg#xywh=160,0,160,90\n";
        assert!(sanitize_webvtt(sprites).is_thumbnail);
    }

    #[test]
    fn resolve_fetches_repairs_and_selects_most_complete_subtitle() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        let dir = temp_dir("sub-prepare");
        let shims = ensure_shims(&dir.join("cache"), &std::env::current_exe().unwrap()).unwrap();

        let mut full_vtt = String::from("WEBVTT\n\n");
        for i in 0..22 {
            let m = i;
            full_vtt.push_str(&format!(
                "{m:02}:10.000 --> {m:02}:12.000\nDialogue line {i}\n\n"
            ));
        }
        full_vtt.push_str(
            "18:09.650 --> 18:12.610\n<b>Edible\n\nReadings</b>\n\n\
             22:26.280 --> 22:30.030\nFinal cue\n",
        );
        let short_vtt = "WEBVTT\n\n00:05.000 --> 00:08.000\nSign 1\n\n\
            01:00.000 --> 01:03.000\nSign 2\n";
        let thumb_vtt = "WEBVTT\n\n00:00.000 --> 00:05.000\nsprite.jpg#xywh=0,0,160,90\n";

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen_referers: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = Arc::clone(&seen_referers);
        let full_body = full_vtt.clone();
        let server = thread::spawn(move || {
            for _ in 0..3 {
                let Ok((mut sock, _)) = listener.accept() else {
                    break;
                };
                let mut reader = BufReader::new(sock.try_clone().unwrap());
                let mut req_line = String::new();
                let _ = reader.read_line(&mut req_line);
                let path = req_line.split_whitespace().nth(1).unwrap_or("");
                loop {
                    let mut hdr = String::new();
                    if reader.read_line(&mut hdr).unwrap_or(0) == 0 || hdr == "\r\n" || hdr == "\n"
                    {
                        break;
                    }
                    if let Some((k, v)) = hdr.split_once(':')
                        && k.eq_ignore_ascii_case("referer")
                    {
                        lock(&seen_clone).push(v.trim().to_string());
                    }
                }
                let body = match path {
                    "/subs/short.vtt" => short_vtt,
                    "/subs/full.vtt" => full_body.as_str(),
                    "/subs/opaque1.vtt" => thumb_vtt,
                    _ => "<!DOCTYPE html><html>not found</html>",
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/vtt\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes());
            }
        });

        let fake = script(
            &dir.join("bin/ani-cli"),
            &format!(
                r#"$ANI_CLI_PLAYER \
  --referrer="https://zokoanime.video/" \
  --sub-file="http://{addr}/subs/short.vtt" \
  --sub-file="http://{addr}/subs/opaque1.vtt" \
  --sub-file="http://{addr}/subs/full.vtt" \
  --force-media-title="Frieren: Beyond Journey's End Episode 1" \
  "https://hls.dramahot.top/p/test/master.m3u8""#
            ),
        );
        let req = build_request(&frieren(), None, "1", Mode::Sub, "best");
        let ResolveOutcome::Stream(stream) =
            resolve_with(&fake, &req, &shims, &[], Duration::from_secs(10), None).unwrap()
        else {
            panic!("expected Stream");
        };
        let _ = server.join();

        let sub_path = PathBuf::from(stream.sub_file.as_deref().expect("cached sub_file"));
        assert!(sub_path.starts_with(dir.join("cache").join("subs")));
        let saved = fs::read_to_string(&sub_path).unwrap();
        assert!(saved.contains("<b>Edible\nReadings</b>"), "{saved}");
        assert!(saved.contains("22:26.280 --> 22:30.030\nFinal cue"));
        assert!(
            lock(&seen_referers)
                .iter()
                .all(|r| r == "https://zokoanime.video/")
                && !lock(&seen_referers).is_empty(),
            "Referer header must be sent on subtitle requests: {:?}",
            *lock(&seen_referers)
        );

        // Unreachable subtitle URL falls back to the original URL instead of failing.
        let offline = script(
            &dir.join("bin/ani-cli-offline"),
            r#"$ANI_CLI_PLAYER \
  --referrer="https://zokoanime.video/" \
  --sub-file="http://127.0.0.1:1/unreachable.vtt" \
  --force-media-title="Frieren: Beyond Journey's End Episode 1" \
  "https://hls.dramahot.top/p/test/master.m3u8""#,
        );
        let ResolveOutcome::Stream(fallback) =
            resolve_with(&offline, &req, &shims, &[], Duration::from_secs(10), None).unwrap()
        else {
            panic!("expected Stream");
        };
        assert_eq!(
            fallback.sub_file.as_deref(),
            Some("http://127.0.0.1:1/unreachable.vtt")
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
