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
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;

use crate::model::{EpisodeRequest, Mode, ShowSummary, Stream};

const RESOLVE_TIMEOUT: Duration = Duration::from_secs(60);
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
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

/// The `ani-cli` executable on PATH, else in `~/.local/bin` or `/usr/local/bin`.
pub fn find() -> Option<PathBuf> {
    let path = env::var_os("PATH").unwrap_or_default();
    env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .chain(env::home_dir().map(|home| home.join(".local/bin")))
        .chain([PathBuf::from("/usr/local/bin")])
        .map(|dir| dir.join("ani-cli"))
        .find(|p| fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// `ani-cli -V`, e.g. "5.1.8".
pub fn version(path: &Path) -> Option<String> {
    let out = run_capture(&[path.into(), "-V".into()], &[], VERSION_TIMEOUT).ok()?;
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
pub fn parse_capture(stdout: &str) -> Result<Stream> {
    let mut url = None;
    let mut referrer = None;
    let mut audio_file = None;
    let mut sub_file = None;
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
            sub_file = value("--sub-file=");
        } else if line.starts_with("--force-media-title=") {
            title = value("--force-media-title=");
        } else if line.starts_with("http") {
            url = Some(line.to_string());
        }
    }
    Ok(Stream {
        url: url.context("ani-cli returned no stream URL")?,
        referrer,
        audio_file,
        sub_file,
        title,
    })
}

/// Resolves the stream for one episode by running ani-cli with the shims in
/// `shims` (see [`ensure_shims`]) as its player and menu. ani-cli records the
/// episode in its own history as it goes.
pub fn resolve(req: &EpisodeRequest, shims: &Path) -> Result<ResolveOutcome> {
    let program = find()
        .context("ani-cli is not installed (looked on PATH, in ~/.local/bin and /usr/local/bin)")?;
    resolve_with(&program, req, shims, &[], RESOLVE_TIMEOUT)
}

/// [`resolve`] with a given ani-cli and extra environment. When the search
/// finds nothing, it is retried once with the last (romaji) title, unless
/// the request already searches for it or picks a result by index.
fn resolve_with(
    program: &Path,
    req: &EpisodeRequest,
    shims: &Path,
    extra_env: &[(&str, OsString)],
    timeout: Duration,
) -> Result<ResolveOutcome> {
    match resolve_once(program, req, shims, extra_env, timeout)? {
        Attempt::Done(outcome) => Ok(outcome),
        Attempt::NoResults(err) => {
            let romaji = req.want_titles.last().filter(|t| **t != req.query);
            match romaji {
                Some(title) if req.select_index.is_none() => {
                    let retry = EpisodeRequest {
                        query: title.clone(),
                        ..req.clone()
                    };
                    match resolve_once(program, &retry, shims, extra_env, timeout)? {
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
) -> Result<Attempt> {
    let mut cmd = vec![program.as_os_str().to_owned(), "--no-detach".into()];
    cmd.extend(episode_args(req));
    let menu_out = menu_out_path(shims);
    let mut env = shim_env(req, shims, &menu_out);
    env.push(("ANI_CLI_PLAYER", shims.join(CAPTURE_NAME).into_os_string()));
    env.extend(extra_env.iter().cloned());
    let run = run_capture(&cmd, &env, timeout);
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
    let stream =
        parse_capture(&out.stdout).map_err(|err| anyhow!("{err}: {}", error_detail(&output)))?;
    Ok(Attempt::Done(ResolveOutcome::Stream(stream)))
}

/// Environment shared by resolve and download runs: the shim dir first on
/// PATH (then `/usr/local/bin` and `~/.local/bin`, for sessions started from a
/// desktop launcher), the menu shim, and what it should pick or report.
pub(crate) fn shim_env(
    req: &EpisodeRequest,
    shims: &Path,
    menu_out: &Path,
) -> Vec<(&'static str, OsString)> {
    let current = env::var_os("PATH").unwrap_or_default();
    let dirs = [shims.to_path_buf(), PathBuf::from("/usr/local/bin")]
        .into_iter()
        .chain(env::home_dir().map(|home| home.join(".local/bin")))
        .chain(env::split_paths(&current));
    let path = env::join_paths(dirs).unwrap_or(current);
    vec![
        ("PATH", path),
        ("ANI_CLI_MENU", shims.join(MENU_NAME).into_os_string()),
        ("ANI_GUI_WANT_TITLES", req.want_titles.join("\n").into()),
        ("ANI_GUI_MENU_OUT", menu_out.as_os_str().to_owned()),
    ]
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

/// `[-S <i>] -e <ep> -q <quality> [--dub] <query>`.
pub(crate) fn episode_args(req: &EpisodeRequest) -> Vec<OsString> {
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
    args.push(req.query.as_str().into());
    args
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
/// carries the output so far.
fn run_capture(cmd: &[OsString], env: &[(&str, OsString)], timeout: Duration) -> Result<Captured> {
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

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            // Killed before reaping, so the group id cannot have been reused.
            if let Ok(pgid) = i32::try_from(child.id()) {
                let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL);
            }
            let _ = child.wait();
            break None;
        }
        thread::sleep(Duration::from_millis(25));
    };
    let stdout = stdout.finish();
    let stderr = stderr.finish();
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
        let err = run_capture(&cmd, &[], Duration::from_secs(1))
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
        )
        .unwrap();
        assert_eq!(ok.stdout, "from-env\n");
        assert_eq!(ok.stderr, "oops\n");
        assert_eq!(ok.status.code(), Some(3));
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
        let outcome = resolve_with(&fake, &req, &shims, &[], Duration::from_secs(10)).unwrap();
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
        let err = resolve_with(&failing, &req, &shims, &[], Duration::from_secs(10))
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
printf '%s|%s|%s|%s\n' "$ANI_CLI_PLAYER" "$ANI_CLI_MENU" "$ANI_GUI_WANT_TITLES" "${{PATH%%:*}}" >> "$log"
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
        let outcome = resolve_with(&fake, &req, &shims, &[], Duration::from_secs(10)).unwrap();
        let ResolveOutcome::Stream(stream) = outcome else {
            panic!("expected a stream, got {outcome:?}");
        };
        assert_eq!(
            stream.url,
            "https://hls.dramahot.top/p/0a1b2c3d4e5f/master.m3u8"
        );
        assert_eq!(stream.referrer.as_deref(), Some("https://zokoanime.video/"));

        let env_line = format!(
            "{}|{}|Frieren: Beyond Journey’s End\nSousou no Frieren|{}",
            shims.join("capture-mpv").display(),
            shims.join("menu").display(),
            shims.display()
        );
        assert_eq!(
            fs::read_to_string(&log).unwrap(),
            format!(
                "--no-detach|-e|1|-q|best|--dub|Frieren: Beyond Journey’s End|{env_line}\n\
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
        let err = resolve_with(&fake, &req, &shims, &[], Duration::from_secs(10))
            .unwrap_err()
            .to_string();
        assert!(err.contains("No results found"), "{err}");
        let picked = EpisodeRequest {
            select_index: Some(2),
            ..build_request(&frieren(), None, "1", Mode::Sub, "best")
        };
        assert!(resolve_with(&fake, &picked, &shims, &[], Duration::from_secs(10)).is_err());
        assert_eq!(
            fs::read_to_string(&log)
                .unwrap()
                .matches("--no-detach")
                .count(),
            2
        );
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
}
