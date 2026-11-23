use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;

use crate::model::{EpisodeRequest, Mode, Stream};
use crate::paths;

const RESOLVE_TIMEOUT: Duration = Duration::from_secs(60);
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
/// How much of ani-cli's output an error message carries.
const DETAIL_CHARS: usize = 240;
/// ani-cli runs the player as `$ANI_CLI_PLAYER`; a name containing "mpv"
/// makes it pass `--referrer=`, `--sub-file=`, … and the URL as arguments.
const SHIM_NAME: &str = "capture-mpv";
const SHIM: &str = "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done\n";

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

/// Writes the capture shim into `cache_dir` (replacing it atomically when its
/// content or mode is off) and returns its path.
pub fn ensure_capture_shim(cache_dir: &Path) -> io::Result<PathBuf> {
    let path = cache_dir.join(SHIM_NAME);
    let current = fs::read(&path).ok();
    let mode = fs::metadata(&path).map(|m| m.permissions().mode() & 0o777);
    if current.as_deref() == Some(SHIM.as_bytes()) && mode.is_ok_and(|m| m == 0o755) {
        return Ok(path);
    }
    fs::create_dir_all(cache_dir)?;
    let tmp = cache_dir.join(format!("{SHIM_NAME}.tmp{}", std::process::id()));
    fs::write(&tmp, SHIM)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
    fs::rename(&tmp, &path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })?;
    Ok(path)
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

/// Resolves the stream for one episode by running ani-cli with the capture
/// shim as its player.
pub fn resolve(req: &EpisodeRequest) -> Result<Stream> {
    let program = find()
        .context("ani-cli is not installed (looked on PATH, in ~/.local/bin and /usr/local/bin)")?;
    let shim = ensure_capture_shim(&paths::cache_dir()).context("writing the capture-mpv shim")?;
    let mut cmd = vec![program.into_os_string(), "--exit-after-play".into()];
    cmd.extend(episode_args(req));
    let env = [
        ("ANI_CLI_PLAYER", shim.into_os_string()),
        ("ANI_CLI_EXTERNAL_MENU", "off".into()),
        ("PATH", child_path()),
    ];
    let out = run_capture(&cmd, &env, RESOLVE_TIMEOUT)?;
    let detail = || error_detail(&format!("{}\n{}", out.stdout, out.stderr));
    if !out.status.success() {
        bail!("ani-cli failed ({}): {}", out.status, detail());
    }
    parse_capture(&out.stdout).map_err(|err| anyhow!("{err}: {}", detail()))
}

/// `-S <nth> -e <ep> -q <quality> [--dub] <query>`.
pub(crate) fn episode_args(req: &EpisodeRequest) -> Vec<OsString> {
    let quality = if req.quality.is_empty() {
        "best"
    } else {
        &req.quality
    };
    let mut args: Vec<OsString> = vec![
        "-S".into(),
        req.nth.to_string().into(),
        "-e".into(),
        req.ep.as_str().into(),
        "-q".into(),
        quality.into(),
    ];
    if req.mode == Mode::Dub {
        args.push("--dub".into());
    }
    args.push(req.query.as_str().into());
    args
}

/// PATH for ani-cli with `/usr/local/bin` and `~/.local/bin` in front, for
/// sessions started from a desktop launcher with a minimal PATH.
pub(crate) fn child_path() -> OsString {
    let current = env::var_os("PATH").unwrap_or_default();
    let dirs = [PathBuf::from("/usr/local/bin")]
        .into_iter()
        .chain(env::home_dir().map(|home| home.join(".local/bin")))
        .chain(env::split_paths(&current));
    env::join_paths(dirs).unwrap_or(current)
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
    let mut child = Command::new(program)
        .args(args)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .with_context(|| format!("starting {}", program.to_string_lossy()))?;
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
    use std::time::Instant;

    use super::*;
    use crate::model::Mode;

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

    #[test]
    fn capture_shim_echoes_args() {
        let dir = std::env::temp_dir().join(format!("ani-gui-shim-{}", std::process::id()));
        let shim = ensure_capture_shim(&dir).unwrap();
        assert_eq!(shim, dir.join("capture-mpv"));
        // Rewriting an existing shim is fine.
        ensure_capture_shim(&dir).unwrap();
        let out = std::process::Command::new(&shim)
            .args(["--referrer=https://r/", "a b", "https://v/x.m3u8"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            "--referrer=https://r/\na b\nhttps://v/x.m3u8\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[ignore = "runs the installed ani-cli against live providers"]
    fn live_resolve_frieren() {
        let stream = resolve(&EpisodeRequest {
            query: "frieren".into(),
            nth: 2,
            ep: "1".into(),
            mode: Mode::Sub,
            quality: "best".into(),
        })
        .unwrap();
        assert!(stream.url.starts_with("http"), "{stream:?}");
        assert!(stream.referrer.is_some(), "{stream:?}");
    }
}
