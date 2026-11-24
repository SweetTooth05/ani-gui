use std::collections::VecDeque;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use nix::fcntl::OFlag;
use nix::libc;
use nix::pty::{Winsize, grantpt, posix_openpt, ptsname_r, unlockpt};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::{Pid, setsid};
use regex::Regex;

use crate::db::{self, Db};
use crate::model::{DlStatus, DownloadRecord, EpisodeRequest, ShowSummary};
use crate::{anicli, paths};

const STALL_AFTER: Duration = Duration::from_secs(60);
/// Downloads that run at once; the rest wait queued. Each runs ani-cli and
/// a downloader against the provider, and ani-cli rewrites its history
/// file on every run, which concurrent runs can clobber.
const MAX_RUNNING: usize = 2;
/// Time a killed download gets to exit on SIGTERM before SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(5);
/// How long output is still read after ani-cli exits, in case something it
/// left behind holds the terminal open.
const EXIT_GRACE: Duration = Duration::from_secs(2);
/// Time `shutdown` gives downloads to exit on SIGTERM before SIGKILL.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
/// Non-progress output lines kept for the error message.
const TAIL_LINES: usize = 40;
/// When ani-cli's results had no title matching the show.
const NEEDS_PICK: &str = "Pick the matching title on the show page first";
const VIDEO_EXTS: [&str; 7] = ["mp4", "mkv", "webm", "avi", "mov", "flv", "m4v"];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DlProgress {
    pub percent: Option<f32>,
    pub speed: Option<String>,
    pub eta: Option<String>,
    pub downloaded: Option<String>,
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid built-in regex")
}

// yt-dlp: "[download]  22.5% of ~123.45MiB at  2.34MiB/s ETA 00:42"
static YTDLP: LazyLock<Regex> =
    LazyLock::new(|| regex(r"\[download\]\s+([\d.]+)%\s+of\s+~?\s*[\d.]+\s*[KMGT]i?B"));
static YTDLP_SPEED: LazyLock<Regex> = LazyLock::new(|| regex(r"at\s+([\d.]+\s*[KMGT]i?B/s)"));
static YTDLP_ETA: LazyLock<Regex> = LazyLock::new(|| regex(r"ETA\s+([\d:]+)"));
// aria2c: "[#abcd 1.2MiB/5.4MiB(22%) CN:16 DL:2.1MiB ETA:2s]"
static ARIA2C: LazyLock<Regex> = LazyLock::new(|| regex(r"\((\d+)%\)"));
static ARIA2C_BYTES: LazyLock<Regex> =
    LazyLock::new(|| regex(r"([\d.]+[KMGT]i?B)/[\d.]+[KMGT]i?B"));
static ARIA2C_SPEED: LazyLock<Regex> = LazyLock::new(|| regex(r"DL:([\d.]+[KMGT]i?B)"));
static ARIA2C_ETA: LazyLock<Regex> = LazyLock::new(|| regex(r"ETA:(\w+)"));
// ffmpeg: "... size=   10240kB time=00:01:23.00 ... speed=1.2x" (KiB since 6.1)
static FFMPEG: LazyLock<Regex> = LazyLock::new(|| regex(r"size=\s*([\d.]+\s*[kKMGT]?i?B)\s"));

/// Percent / speed / ETA from one line of downloader output (yt-dlp, aria2c or
/// ffmpeg, whichever ani-cli used). ffmpeg only reports bytes. None for any
/// other line.
pub fn parse_progress_line(line: &str) -> Option<DlProgress> {
    let line = anicli::strip_ansi(line);
    let line = line.trim();
    let group = |re: &Regex| {
        re.captures(line)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().replace(' ', ""))
    };

    if let Some(percent) = group(&YTDLP) {
        return Some(DlProgress {
            percent: percent.parse().ok(),
            speed: group(&YTDLP_SPEED),
            eta: group(&YTDLP_ETA),
            downloaded: None,
        });
    }
    if let Some(percent) = group(&ARIA2C).filter(|_| line.contains("DL:") || line.contains("ETA:"))
    {
        return Some(DlProgress {
            percent: percent.parse().ok(),
            speed: group(&ARIA2C_SPEED).map(|s| s + "/s"),
            eta: group(&ARIA2C_ETA),
            downloaded: group(&ARIA2C_BYTES),
        });
    }
    if line.contains("time=") {
        return group(&FFMPEG).map(|size| DlProgress {
            downloaded: Some(size),
            ..Default::default()
        });
    }
    None
}

/// Runs `ani-cli -d` downloads in the background, keeps their records (and
/// live progress) in memory and persists every status change.
pub struct DownloadManager {
    inner: Arc<Inner>,
}

struct Inner {
    db: Arc<Mutex<Db>>,
    /// ani-cli to run; None looks it up per download.
    program: Option<PathBuf>,
    /// Where `anicli::ensure_shims` puts the menu shim.
    shim_cache: PathBuf,
    stall_after: Duration,
    items: Mutex<Vec<Item>>,
    /// Lock after `items` when both are needed, never before.
    queue: Mutex<Queue>,
    on_change: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Set by `shutdown`; workers then leave records as it saved them.
    closing: AtomicBool,
}

/// Downloads waiting for a worker, oldest first, and how many workers run.
#[derive(Default)]
struct Queue {
    waiting: VecDeque<String>,
    running: usize,
}

/// One download, newest first in `Inner::items`.
struct Item {
    record: DownloadRecord,
    progress: Option<DlProgress>,
    /// What to run; None for records from an earlier session.
    job: Option<Job>,
    /// Process group of the running ani-cli.
    pgid: Option<Pid>,
    cancel: bool,
}

#[derive(Clone)]
struct Job {
    req: EpisodeRequest,
    dir: PathBuf,
}

enum Outcome {
    Done(PathBuf),
    Failed(String),
    Cancelled,
}

impl DownloadManager {
    /// Loads saved downloads; ones a previous session left queued or running
    /// are marked failed.
    pub fn new(db: Arc<Mutex<Db>>) -> Self {
        Self::with_runner(db, None, paths::cache_dir(), STALL_AFTER)
    }

    fn with_runner(
        db: Arc<Mutex<Db>>,
        program: Option<PathBuf>,
        shim_cache: PathBuf,
        stall_after: Duration,
    ) -> Self {
        let records = lock(&db).list_downloads().unwrap_or_else(|err| {
            eprintln!("ani-gui: loading downloads: {err:#}");
            Vec::new()
        });
        let inner = Inner {
            db,
            program,
            shim_cache,
            stall_after,
            items: Mutex::new(Vec::new()),
            queue: Mutex::new(Queue::default()),
            on_change: Mutex::new(None),
            closing: AtomicBool::new(false),
        };
        let items = records
            .into_iter()
            .map(|mut record| {
                if matches!(record.status, DlStatus::Queued | DlStatus::Running) {
                    record.status = DlStatus::Failed;
                    record.error = Some("interrupted".into());
                    inner.persist(&record);
                }
                Item {
                    record,
                    progress: None,
                    job: None,
                    pgid: None,
                    cancel: false,
                }
            })
            .collect();
        *lock(&inner.items) = items;
        DownloadManager {
            inner: Arc::new(inner),
        }
    }

    /// Queues `req` for `show` into `dir`; it starts once fewer than
    /// `MAX_RUNNING` downloads run. Refuses an episode that is not a plain
    /// number (see `anicli::check_episode`).
    pub fn start(&self, show: &ShowSummary, req: EpisodeRequest, dir: &Path) -> Result<String> {
        anicli::check_episode(&req.ep)?;
        let id = new_id();
        let record = DownloadRecord {
            id: id.clone(),
            show_id: show.id.clone(),
            title: show.display_title().to_string(),
            ep: req.ep.clone(),
            mode: req.mode,
            quality: req.quality.clone(),
            path: None,
            status: DlStatus::Queued,
            progress: 0.0,
            error: None,
            created_at: db::now(),
        };
        {
            let mut items = lock(&self.inner.items);
            self.inner.persist(&record);
            items.insert(
                0,
                Item {
                    record,
                    progress: None,
                    job: Some(Job {
                        req,
                        dir: dir.to_path_buf(),
                    }),
                    pgid: None,
                    cancel: false,
                },
            );
        }
        self.inner.notify();
        self.inner.clone().enqueue(id.clone());
        Ok(id)
    }

    /// Stops a queued or running download (its whole process group); the
    /// partial file stays on disk. One still waiting is just marked cancelled.
    pub fn cancel(&self, id: &str) {
        {
            let mut items = lock(&self.inner.items);
            let Some(item) = items.iter_mut().find(|i| i.record.id == id) else {
                return;
            };
            if !self.inner.dequeue(id) {
                request_stop(item);
                return;
            }
            item.record.status = DlStatus::Cancelled;
            item.progress = None;
            self.inner.persist(&item.record);
        }
        self.inner.notify();
    }

    /// Runs a failed or cancelled download again under the same id.
    pub fn retry(&self, id: &str) -> Result<()> {
        {
            let mut items = lock(&self.inner.items);
            let item = items
                .iter_mut()
                .find(|i| i.record.id == id)
                .ok_or_else(|| anyhow!("download not found"))?;
            if !matches!(item.record.status, DlStatus::Failed | DlStatus::Cancelled) {
                bail!("only failed or cancelled downloads can be retried");
            }
            if item.job.is_none() {
                bail!(
                    "this download is from an earlier session; start it again from the show page"
                );
            }
            item.record.status = DlStatus::Queued;
            item.record.progress = 0.0;
            item.record.error = None;
            item.record.path = None;
            item.progress = None;
            item.cancel = false;
            self.inner.persist(&item.record);
        }
        self.inner.notify();
        self.inner.clone().enqueue(id.to_string());
        Ok(())
    }

    /// Forgets a download (stopping it if active); files on disk stay.
    pub fn remove(&self, id: &str) {
        {
            let mut items = lock(&self.inner.items);
            let Some(pos) = items.iter().position(|i| i.record.id == id) else {
                return;
            };
            if !self.inner.dequeue(id) {
                request_stop(&mut items[pos]);
            }
            items.remove(pos);
            if let Err(err) = lock(&self.inner.db).delete_download(id) {
                eprintln!("ani-gui: deleting download {id}: {err:#}");
            }
        }
        self.inner.notify();
    }

    /// Newest first, with live progress for running downloads.
    pub fn snapshot(&self) -> Vec<(DownloadRecord, Option<DlProgress>)> {
        lock(&self.inner.items)
            .iter()
            .map(|i| (i.record.clone(), i.progress.clone()))
            .collect()
    }

    /// For app exit: records every active download as failed ("app closed")
    /// and stops its process group, SIGTERM first and SIGKILL if it outlives
    /// a short grace.
    pub fn shutdown(&self) {
        let ids: Vec<String> = {
            let mut items = lock(&self.inner.items);
            self.inner.closing.store(true, Ordering::SeqCst);
            lock(&self.inner.queue).waiting.clear();
            items
                .iter_mut()
                .filter(|i| matches!(i.record.status, DlStatus::Queued | DlStatus::Running))
                .map(|item| {
                    item.cancel = true;
                    item.progress = None;
                    item.record.status = DlStatus::Failed;
                    item.record.error = Some("app closed".into());
                    self.inner.persist(&item.record);
                    item.record.id.clone()
                })
                .collect()
        };
        self.inner.notify();
        if !self.inner.signal_groups(&ids, Some(Signal::SIGTERM)) {
            return;
        }
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
            if !self.inner.signal_groups(&ids, None) {
                return;
            }
        }
        self.inner.signal_groups(&ids, Some(Signal::SIGKILL));
    }

    /// Called (from a worker thread) after every change to the snapshot.
    pub fn set_on_change(&self, f: Box<dyn Fn() + Send + Sync>) {
        *lock(&self.inner.on_change) = Some(Arc::from(f));
    }
}

/// Marks an active download cancelled and SIGTERMs its process group; the
/// worker escalates to SIGKILL and records the outcome.
fn request_stop(item: &mut Item) {
    if matches!(item.record.status, DlStatus::Queued | DlStatus::Running) {
        item.cancel = true;
        if let Some(pgid) = item.pgid {
            let _ = killpg(pgid, Signal::SIGTERM);
        }
    }
}

impl Inner {
    fn enqueue(self: Arc<Self>, id: String) {
        lock(&self.queue).waiting.push_back(id);
        self.pump();
    }

    /// Removes `id` from the waiting downloads; false if it wasn't waiting.
    fn dequeue(&self, id: &str) -> bool {
        let mut queue = lock(&self.queue);
        let before = queue.waiting.len();
        queue.waiting.retain(|w| w != id);
        queue.waiting.len() != before
    }

    /// Starts waiting downloads while fewer than `MAX_RUNNING` run.
    fn pump(self: Arc<Self>) {
        loop {
            let id = {
                let mut queue = lock(&self.queue);
                if self.closing.load(Ordering::SeqCst) {
                    queue.waiting.clear();
                    return;
                }
                if queue.running >= MAX_RUNNING {
                    return;
                }
                let Some(id) = queue.waiting.pop_front() else {
                    return;
                };
                queue.running += 1;
                id
            };
            let inner = self.clone();
            thread::spawn(move || {
                let outcome = inner.run(&id);
                inner.finish(&id, outcome);
                lock(&inner.queue).running -= 1;
                inner.pump();
            });
        }
    }

    fn run(&self, id: &str) -> Outcome {
        let job = {
            let items = lock(&self.items);
            match items.iter().find(|i| i.record.id == id) {
                Some(item) if !item.cancel => item.job.clone(),
                _ => return Outcome::Cancelled,
            }
        };
        let Some(Job { req, dir }) = job else {
            return Outcome::Failed("nothing to download".into());
        };
        let Some(program) = self.program.clone().or_else(anicli::find) else {
            return Outcome::Failed("ani-cli is not installed".into());
        };
        if let Err(err) = fs::create_dir_all(&dir) {
            return Outcome::Failed(format!("creating {}: {err}", dir.display()));
        }
        let shims =
            match env::current_exe().and_then(|exe| anicli::ensure_shims(&self.shim_cache, &exe)) {
                Ok(shims) => shims,
                Err(err) => return Outcome::Failed(format!("writing ani-cli shims: {err}")),
            };
        let menu_out = anicli::menu_out_path(&shims);
        // Coarse filesystem timestamps can trail the clock a little.
        let started = SystemTime::now()
            .checked_sub(Duration::from_secs(2))
            .unwrap_or(UNIX_EPOCH);
        let (mut child, master) = match spawn_in_pty(&program, &req, &dir, &shims, &menu_out) {
            Ok(spawned) => spawned,
            Err(err) => return Outcome::Failed(format!("starting ani-cli: {err}")),
        };
        // setsid made the child the leader of its own process group.
        let pgid = Pid::from_raw(i32::try_from(child.id()).unwrap_or(i32::MAX));
        {
            let mut items = lock(&self.items);
            if self.closing.load(Ordering::SeqCst) {
                // shutdown() already settled this record but couldn't see the group.
                let _ = killpg(pgid, Signal::SIGKILL);
            } else if let Some(item) = items.iter_mut().find(|i| i.record.id == id) {
                item.record.status = DlStatus::Running;
                item.pgid = Some(pgid);
                self.persist(&item.record);
            }
        }
        self.notify();

        let tail = self.watch(id, pgid, master, &mut child);
        let status = child.wait();
        let needs_pick = anicli::take_menu_out(&menu_out).is_some();
        let (cancelled, title) = {
            let mut items = lock(&self.items);
            match items.iter_mut().find(|i| i.record.id == id) {
                Some(item) => {
                    item.pgid = None;
                    (item.cancel, item.record.title.clone())
                }
                None => (true, String::new()),
            }
        };
        match (tail, status) {
            _ if cancelled => Outcome::Cancelled,
            _ if needs_pick => Outcome::Failed(NEEDS_PICK.into()),
            (Watched::Stalled, _) => Outcome::Failed("stalled".into()),
            (Watched::Exited(_), Ok(status)) if status.success() => {
                match find_download(&dir, &title, &req.ep, started) {
                    Some(path) => Outcome::Done(path),
                    None => Outcome::Failed(format!(
                        "ani-cli finished, but no file for episode {} is in {}",
                        req.ep,
                        dir.display()
                    )),
                }
            }
            (Watched::Exited(tail), Ok(status)) => Outcome::Failed(failure_detail(&tail, status)),
            (_, Err(err)) => Outcome::Failed(format!("waiting for ani-cli: {err}")),
        }
    }

    /// Reads the download's terminal until every process holding it is gone
    /// (or shortly after `child` exits), updating progress, and kills the
    /// group on cancel or stall.
    fn watch(&self, id: &str, pgid: Pid, master: File, child: &mut Child) -> Watched {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        thread::spawn(move || {
            let mut master = master;
            let mut chunk = [0u8; 4096];
            loop {
                match master.read(&mut chunk) {
                    // EIO once the last slave fd closes.
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(chunk[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        });

        let mut pending = Vec::new();
        let mut tail = VecDeque::new();
        let mut last = DlProgress::default();
        let mut advanced_at = Instant::now();
        let mut stalled = false;
        let mut term_sent: Option<Instant> = None;
        let mut kill_sent = false;
        let mut exited_at: Option<Instant> = None;
        let mut handle = |line: &[u8], last: &mut DlProgress, advanced_at: &mut Instant| {
            let line = String::from_utf8_lossy(line);
            if let Some(p) = parse_progress_line(&line) {
                let forward = p
                    .percent
                    .zip(last.percent)
                    .map_or(p.percent.is_some(), |(new, old)| new > old)
                    || (p.downloaded.is_some() && p.downloaded != last.downloaded);
                if forward {
                    *advanced_at = Instant::now();
                }
                *last = p.clone();
                self.set_progress(id, p);
            } else {
                let line = anicli::strip_ansi(&line);
                if !line.trim().is_empty() {
                    tail.push_back(line);
                    if tail.len() > TAIL_LINES {
                        tail.pop_front();
                    }
                }
            }
        };
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(chunk) => {
                    pending.extend_from_slice(&chunk);
                    while let Some(end) = pending.iter().position(|&b| b == b'\n' || b == b'\r') {
                        let line: Vec<u8> = pending.drain(..=end).collect();
                        handle(&line[..end], &mut last, &mut advanced_at);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if exited_at.is_none() && self.reap(id, child) {
                exited_at = Some(Instant::now());
            }
            // Once reaped the group id may be reused, so no more signals; a
            // straggler still holding the terminal gets a moment to flush.
            if let Some(at) = exited_at {
                if at.elapsed() >= EXIT_GRACE {
                    break;
                }
                continue;
            }
            match term_sent {
                None => {
                    stalled = advanced_at.elapsed() >= self.stall_after;
                    if stalled || self.cancel_requested(id) {
                        let _ = killpg(pgid, Signal::SIGTERM);
                        term_sent = Some(Instant::now());
                    }
                }
                Some(at) if !kill_sent && at.elapsed() >= KILL_GRACE => {
                    let _ = killpg(pgid, Signal::SIGKILL);
                    kill_sent = true;
                }
                Some(_) => {}
            }
        }
        if !pending.is_empty() {
            handle(&pending, &mut last, &mut advanced_at);
        }
        if stalled {
            Watched::Stalled
        } else {
            Watched::Exited(Vec::from(tail).join("\n"))
        }
    }

    /// Whether `child` has exited. Reaps it and forgets its group under the
    /// items lock, so nobody signals a group id that may have been reused.
    fn reap(&self, id: &str, child: &mut Child) -> bool {
        let mut items = lock(&self.items);
        let exited = matches!(child.try_wait(), Ok(Some(_)));
        if exited && let Some(item) = items.iter_mut().find(|i| i.record.id == id) {
            item.pgid = None;
        }
        exited
    }

    /// Sends `signal` (None only probes) to the groups of `ids` not yet
    /// reaped; true if any of them still exists.
    fn signal_groups(&self, ids: &[String], signal: Option<Signal>) -> bool {
        let items = lock(&self.items);
        let mut any = false;
        for item in items.iter().filter(|i| ids.contains(&i.record.id)) {
            if let Some(pgid) = item.pgid {
                any |= killpg(pgid, signal).is_ok();
            }
        }
        any
    }

    fn cancel_requested(&self, id: &str) -> bool {
        lock(&self.items)
            .iter()
            .find(|i| i.record.id == id)
            .is_none_or(|i| i.cancel)
    }

    fn set_progress(&self, id: &str, p: DlProgress) {
        {
            let mut items = lock(&self.items);
            if self.closing.load(Ordering::SeqCst) {
                return;
            }
            let Some(item) = items.iter_mut().find(|i| i.record.id == id) else {
                return;
            };
            if let Some(percent) = p.percent {
                item.record.progress = percent;
            }
            item.progress = Some(p);
        }
        self.notify();
    }

    fn finish(&self, id: &str, outcome: Outcome) {
        {
            let mut items = lock(&self.items);
            if self.closing.load(Ordering::SeqCst) {
                return;
            }
            let Some(item) = items.iter_mut().find(|i| i.record.id == id) else {
                return;
            };
            let record = &mut item.record;
            match outcome {
                Outcome::Done(path) => {
                    record.status = DlStatus::Done;
                    record.progress = 100.0;
                    record.path = Some(path.to_string_lossy().into_owned());
                    record.error = None;
                }
                Outcome::Failed(error) => {
                    record.status = DlStatus::Failed;
                    record.error = Some(error);
                }
                Outcome::Cancelled => record.status = DlStatus::Cancelled,
            }
            item.progress = None;
            item.cancel = false;
            self.persist(&item.record);
        }
        self.notify();
    }

    fn persist(&self, record: &DownloadRecord) {
        if let Err(err) = lock(&self.db).upsert_download(record) {
            eprintln!("ani-gui: saving download {}: {err:#}", record.id);
        }
    }

    fn notify(&self) {
        let on_change = lock(&self.on_change).clone();
        if let Some(f) = on_change {
            f();
        }
    }
}

enum Watched {
    /// Output ended; the non-progress tail of it.
    Exited(String),
    Stalled,
}

/// Starts `ani-cli -d …` in a new session on a fresh pseudo-terminal (so
/// yt-dlp/ffmpeg print live progress) and returns the pty's master side.
fn spawn_in_pty(
    program: &Path,
    req: &EpisodeRequest,
    dir: &Path,
    shims: &Path,
    menu_out: &Path,
) -> io::Result<(Child, File)> {
    let args = anicli::episode_args(req).map_err(io::Error::other)?;
    let (master, slave) = open_pty()?;
    let mut cmd = Command::new(program);
    cmd.arg("-d")
        .args(args)
        .envs(anicli::shim_env(req, shims, menu_out))
        .env("ANI_CLI_DOWNLOAD_DIR", dir)
        .stdin(Stdio::null())
        .stdout(slave.try_clone()?)
        .stderr(slave.try_clone()?);
    // SAFETY: setsid is async-signal-safe and touches no parent state.
    unsafe {
        cmd.pre_exec(|| setsid().map(drop).map_err(io::Error::from));
    }
    let child = anicli::spawn(&mut cmd)?;
    // The master only reports EOF once no process holds a slave fd, ours included.
    drop(cmd);
    drop(slave);
    Ok((child, File::from(master)))
}

/// A 24x100 pseudo-terminal whose two fds are close-on-exec from the start,
/// so children spawned by other threads never inherit them.
fn open_pty() -> io::Result<(OwnedFd, File)> {
    let master = posix_openpt(OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_CLOEXEC)?;
    grantpt(&master)?;
    unlockpt(&master)?;
    // std opens files with O_CLOEXEC.
    let slave = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(ptsname_r(&master)?)?;
    let size = Winsize {
        ws_row: 24,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ only reads the winsize behind the valid pointer.
    if unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCSWINSZ, &size) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok((master.into(), slave))
}

fn failure_detail(tail: &str, status: ExitStatus) -> String {
    let detail = anicli::error_detail(tail);
    if detail.is_empty() {
        format!("ani-cli failed ({status})")
    } else {
        detail
    }
}

fn new_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{nanos:x}-{:x}", NEXT.fetch_add(1, Ordering::Relaxed))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The newest video file in `dir` for episode `ep`. ani-cli names downloads
/// "<its own title> Episode <ep>.mp4", and its title can differ from ours, so
/// a file whose name also contains `title` wins, else any for the episode
/// written at or after `since` (so never an older file of another show).
fn find_download(dir: &Path, title: &str, ep: &str, since: SystemTime) -> Option<PathBuf> {
    let episode = Regex::new(&format!(
        r"episode\s*0*{}(?:\D|$)",
        regex::escape(&ep.to_lowercase())
    ))
    .ok()?;
    let mut files: Vec<(SystemTime, SystemTime, PathBuf, String)> = fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            let ext = path.extension()?.to_str()?.to_lowercase();
            if !VIDEO_EXTS.contains(&ext.as_str()) {
                return None;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !episode.is_match(&name.to_lowercase()) {
                return None;
            }
            let meta = entry.metadata().ok()?;
            let modified = meta.modified().ok()?;
            // A downloader may backdate mtime to the server's; ctime it can't.
            let changed = u64::try_from(meta.ctime())
                .map_or(UNIX_EPOCH, |secs| UNIX_EPOCH + Duration::from_secs(secs));
            Some((modified, modified.max(changed), path, name))
        })
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    let title = norm_name(title);
    let titled = files
        .iter()
        .position(|(_, _, _, name)| !title.is_empty() && norm_name(name).contains(&title));
    let pick = titled.or_else(|| {
        files
            .iter()
            .position(|(_, changed, _, _)| *changed >= since)
    })?;
    Some(files.swap_remove(pick).2)
}

/// Mirrors how ani-cli sanitises titles into filenames: text before the
/// first '(', punctuation dropped, whitespace collapsed, lowercased.
fn norm_name(s: &str) -> String {
    let s = s.split('(').next().unwrap_or_default();
    let kept: String = s
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect();
    kept.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    use super::*;
    use crate::model::{DlStatus, Mode};

    fn pct(
        percent: f32,
        speed: Option<&str>,
        eta: Option<&str>,
        downloaded: Option<&str>,
    ) -> DlProgress {
        DlProgress {
            percent: Some(percent),
            speed: speed.map(str::to_string),
            eta: eta.map(str::to_string),
            downloaded: downloaded.map(str::to_string),
        }
    }

    #[test]
    fn parse_progress_ytdlp() {
        assert_eq!(
            parse_progress_line("[download]  22.5% of ~123.45MiB at  2.34MiB/s ETA 00:42"),
            Some(pct(22.5, Some("2.34MiB/s"), Some("00:42"), None))
        );
        assert_eq!(
            parse_progress_line(
                "[download]  97.1% of ~ 812.30MiB at    5.12 MiB/s ETA 01:02:03 (frag 290/300)"
            ),
            Some(pct(97.1, Some("5.12MiB/s"), Some("01:02:03"), None))
        );
        assert_eq!(
            parse_progress_line("[download] 100% of  350.00MiB in 00:01:10 at 4.99MiB/s"),
            Some(pct(100.0, Some("4.99MiB/s"), None, None))
        );
        // On a terminal yt-dlp colours the numbers.
        assert_eq!(
            parse_progress_line(
                "\x1b[K[download] \x1b[0;94m 22.5%\x1b[0m of ~ 123.45MiB at \x1b[0;32m  2.34MiB/s\x1b[0m ETA \x1b[0;33m00:42\x1b[0m (frag 3/40)"
            ),
            Some(pct(22.5, Some("2.34MiB/s"), Some("00:42"), None))
        );
    }

    #[test]
    fn parse_progress_aria2c() {
        assert_eq!(
            parse_progress_line("[#abcd12 1.2MiB/5.4MiB(22%) CN:16 DL:2.1MiB ETA:2s]"),
            Some(pct(22.0, Some("2.1MiB/s"), Some("2s"), Some("1.2MiB")))
        );
    }

    #[test]
    fn parse_progress_ffmpeg() {
        let expect = |downloaded: &str| DlProgress {
            downloaded: Some(downloaded.into()),
            ..Default::default()
        };
        assert_eq!(
            parse_progress_line(
                "frame= 2001 fps=0.0 q=-1.0 size=   10240kB time=00:01:23.00 bitrate=1010.6kbits/s speed=1.2x"
            ),
            Some(expect("10240kB"))
        );
        // ffmpeg >= 6.1 prints binary units.
        assert_eq!(
            parse_progress_line(
                "size=  204800KiB time=00:20:00.12 bitrate=1398.1kbits/s speed=24x    "
            ),
            Some(expect("204800KiB"))
        );
    }

    #[test]
    fn parse_progress_ignores_noise() {
        for line in [
            "",
            "   ",
            "\x1b[2K\x1b[1;34mChecking dependencies...\x1b[0m",
            "[hlsnative] Downloading m3u8 manifest",
            "[download] Destination: Frieren Episode 1.mp4",
            "[info] Downloading 1 format(s): 0",
            "size=N/A",
            "Input #0, hls, from 'https://x/master.m3u8':",
            "(42%) without any downloader markers",
        ] {
            assert_eq!(parse_progress_line(line), None, "{line:?}");
        }
    }

    #[test]
    fn find_download_matches_episode_token() {
        let dir = temp_dir("find");
        fs::create_dir_all(&dir).unwrap();
        for name in [
            "Frieren Beyond Journeys End Episode 1.mp4",
            "Frieren Beyond Journeys End Episode 10.mp4",
            "Frieren Beyond Journeys End Episode 1.vtt",
            "Other Show Episode 2.mkv",
        ] {
            fs::write(dir.join(name), b"x").unwrap();
        }
        let later = SystemTime::now() + Duration::from_secs(60);
        // A title match counts whenever the file was written.
        assert_eq!(
            find_download(&dir, "Frieren: Beyond Journey's End", "1", later),
            Some(dir.join("Frieren Beyond Journeys End Episode 1.mp4"))
        );
        // Without a title match only a file written since the download began
        // counts, never an older one from another show.
        assert_eq!(find_download(&dir, "Sousou no Frieren", "2", later), None);
        assert_eq!(
            find_download(&dir, "Sousou no Frieren", "2", UNIX_EPOCH),
            Some(dir.join("Other Show Episode 2.mkv"))
        );
        assert_eq!(find_download(&dir, "Frieren", "3", UNIX_EPOCH), None);
        assert_eq!(
            find_download(&dir.join("missing"), "Frieren", "1", UNIX_EPOCH),
            None
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(name: &str) -> PathBuf {
        let n = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("ani-gui-dl-{name}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// A stand-in for ani-cli: `body` runs under `sh` with the real arguments.
    fn fake_ani_cli(dir: &Path, body: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join("fake-ani-cli");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn show() -> ShowSummary {
        ShowSummary {
            id: "ReHMC7TQnch3C6z8j".into(),
            title: "Sousou no Frieren".into(),
            english_title: Some("Frieren: Beyond Journey's End".into()),
            sub_count: 28,
            dub_count: 28,
            ..Default::default()
        }
    }

    fn req() -> EpisodeRequest {
        EpisodeRequest {
            query: "Frieren: Beyond Journey's End".into(),
            want_titles: vec![
                "Frieren: Beyond Journey's End".into(),
                "Sousou no Frieren".into(),
            ],
            select_index: Some(1),
            ep: "3".into(),
            mode: Mode::Dub,
            quality: "720".into(),
        }
    }

    /// The fake ani-cli lives in `<dir>/bin`; shims go under `<dir>/cache`.
    fn manager(program: PathBuf, stall_after: Duration) -> (DownloadManager, Arc<Mutex<Db>>) {
        let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
        let cache = program.parent().unwrap().parent().unwrap().join("cache");
        let m = DownloadManager::with_runner(db.clone(), Some(program), cache, stall_after);
        (m, db)
    }

    fn wait_for(
        m: &DownloadManager,
        id: &str,
        done: impl Fn(&DownloadRecord) -> bool,
    ) -> DownloadRecord {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let rec = m
                .snapshot()
                .into_iter()
                .find(|(r, _)| r.id == id)
                .map(|(r, _)| r);
            if let Some(rec) = rec.filter(|r| done(r)) {
                return rec;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {id}: {:?}",
                m.snapshot()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn finished(r: &DownloadRecord) -> bool {
        matches!(
            r.status,
            DlStatus::Done | DlStatus::Failed | DlStatus::Cancelled
        )
    }

    #[test]
    fn download_runs_to_done_under_pty() {
        let dir = temp_dir("done");
        let script = fake_ani_cli(
            &dir.join("bin"),
            r#"printf '%s\n' "$@" > "$ANI_CLI_DOWNLOAD_DIR/args.txt"
printf '%s|%s|%s\n' "$ANI_CLI_MENU" "$ANI_GUI_WANT_TITLES" "${PATH%%:*}" > "$ANI_CLI_DOWNLOAD_DIR/env.txt"
test -t 1 && echo yes > "$ANI_CLI_DOWNLOAD_DIR/tty.txt"
printf '[download]  10.0%% of ~100.00MiB at  1.00MiB/s ETA 01:30\r'
printf '[download]  55.5%% of ~100.00MiB at  2.00MiB/s ETA 00:20\r'
echo x > "$ANI_CLI_DOWNLOAD_DIR/Frieren Beyond Journeys End Episode 3.mp4"
printf '[download] 100%% of  100.00MiB in 00:00:40 at 2.50MiB/s\n'"#,
        );
        let (m, db) = manager(script, Duration::from_secs(60));
        let changes = Arc::new(AtomicUsize::new(0));
        let counter = changes.clone();
        m.set_on_change(Box::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        }));
        let out = dir.join("out");
        let id = m.start(&show(), req(), &out).unwrap();

        let rec = wait_for(&m, &id, finished);
        assert_eq!(rec.status, DlStatus::Done, "{rec:?}");
        assert_eq!(
            rec.path.as_deref(),
            out.join("Frieren Beyond Journeys End Episode 3.mp4")
                .to_str()
        );
        assert_eq!(rec.progress, 100.0);
        assert_eq!(
            (rec.show_id.as_str(), rec.ep.as_str(), rec.mode),
            ("ReHMC7TQnch3C6z8j", "3", Mode::Dub)
        );
        assert_eq!(rec.quality, "720");
        assert_eq!(
            fs::read_to_string(out.join("args.txt")).unwrap(),
            "-d\n-S\n1\n-e\n3\n-q\n720\n--dub\nFrieren%3A Beyond Journey%27s End\n"
        );
        let shims = dir.join("cache").join("shims");
        assert_eq!(
            fs::read_to_string(out.join("env.txt")).unwrap(),
            format!(
                "{}|Frieren: Beyond Journey's End\nSousou no Frieren|{}\n",
                shims.join("menu").display(),
                shims.display()
            )
        );
        assert_eq!(fs::read_to_string(out.join("tty.txt")).unwrap(), "yes\n");
        assert!(changes.load(Ordering::Relaxed) >= 2);
        assert_eq!(db.lock().unwrap().list_downloads().unwrap(), [rec]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn download_failure_reports_detail_then_retry_succeeds() {
        let dir = temp_dir("fail");
        let script = fake_ani_cli(
            &dir.join("bin"),
            r#"if [ ! -e "$ANI_CLI_DOWNLOAD_DIR/second" ]; then
  touch "$ANI_CLI_DOWNLOAD_DIR/second"
  printf '\033[1;31mEpisode is released, but no valid sources!\033[0m\n' >&2
  exit 1
fi
echo x > "$ANI_CLI_DOWNLOAD_DIR/Frieren Episode 3.mkv""#,
        );
        let (m, db) = manager(script, Duration::from_secs(60));
        let out = dir.join("out");
        let id = m.start(&show(), req(), &out).unwrap();

        let rec = wait_for(&m, &id, finished);
        assert_eq!(rec.status, DlStatus::Failed);
        assert_eq!(
            rec.error.as_deref(),
            Some("Episode is released, but no valid sources!")
        );
        assert_eq!(
            db.lock().unwrap().list_downloads().unwrap()[0].status,
            DlStatus::Failed
        );

        m.retry(&id).unwrap();
        let rec = wait_for(&m, &id, |r| r.status == DlStatus::Done);
        assert_eq!(rec.error, None);
        assert_eq!(
            rec.path.as_deref(),
            out.join("Frieren Episode 3.mkv").to_str()
        );
        assert!(
            m.retry(&id).is_err(),
            "only failed or cancelled downloads retry"
        );
        assert!(m.retry("nope").is_err());

        m.remove(&id);
        assert!(m.snapshot().is_empty());
        assert!(db.lock().unwrap().list_downloads().unwrap().is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cancel_kills_process_group() {
        let dir = temp_dir("cancel");
        let script = fake_ani_cli(
            &dir.join("bin"),
            r#"sleep 30 &
echo $! > "$ANI_CLI_DOWNLOAD_DIR/child.pid"
printf '[download]   1.0%% of ~100.00MiB at  1.00MiB/s ETA 09:00\n'
wait"#,
        );
        let (m, _db) = manager(script, Duration::from_secs(60));
        let out = dir.join("out");
        let id = m.start(&show(), req(), &out).unwrap();
        let (_, progress) = {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let snap = m.snapshot().into_iter().find(|(r, _)| r.id == id).unwrap();
                if snap.1.as_ref().and_then(|p| p.percent) == Some(1.0) {
                    break snap;
                }
                assert!(Instant::now() < deadline, "no progress: {snap:?}");
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        assert_eq!(progress.unwrap().eta.as_deref(), Some("09:00"));

        m.cancel(&id);
        let rec = wait_for(&m, &id, finished);
        assert_eq!(rec.status, DlStatus::Cancelled);
        let child: i32 = fs::read_to_string(out.join("child.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while nix::sys::signal::kill(nix::unistd::Pid::from_raw(child), None).is_ok() {
            assert!(
                Instant::now() < deadline,
                "grandchild {child} survived cancel"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn start_rejects_bad_episode_without_running_ani_cli() {
        let dir = temp_dir("bad-ep");
        let ran = dir.join("ran");
        let script = fake_ani_cli(&dir.join("bin"), &format!("touch '{}'", ran.display()));
        let (m, db) = manager(script, Duration::from_secs(60));
        let bad = EpisodeRequest {
            ep: "1\\|x$|touch FILE|e;#".into(),
            ..req()
        };
        let err = m.start(&show(), bad, &dir.join("out")).unwrap_err();
        assert!(err.to_string().contains("not a valid episode"), "{err}");
        std::thread::sleep(Duration::from_millis(300));
        assert!(!ran.exists(), "ani-cli ran for an invalid episode");
        assert!(m.snapshot().is_empty());
        assert!(db.lock().unwrap().list_downloads().unwrap().is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A fake ani-cli that logs its episode to `<dir>/started.log`, then
    /// waits for `<dir>/release` before finishing the download.
    fn gated_ani_cli(dir: &Path) -> PathBuf {
        fake_ani_cli(
            &dir.join("bin"),
            &format!(
                r#"while [ $# -gt 0 ]; do [ "$1" = -e ] && ep=$2; shift; done
echo "$ep" >> '{log}'
while [ ! -e '{release}' ]; do sleep 0.05; done
echo x > "$ANI_CLI_DOWNLOAD_DIR/Frieren Episode $ep.mp4""#,
                log = dir.join("started.log").display(),
                release = dir.join("release").display(),
            ),
        )
    }

    fn started_eps(dir: &Path) -> Vec<String> {
        let mut eps: Vec<String> = fs::read_to_string(dir.join("started.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        eps.sort();
        eps
    }

    fn start_ep(m: &DownloadManager, dir: &Path, ep: u32) -> String {
        let req = EpisodeRequest {
            ep: ep.to_string(),
            ..req()
        };
        m.start(&show(), req, &dir.join(format!("out{ep}")))
            .unwrap()
    }

    fn status_of(m: &DownloadManager, id: &str) -> DlStatus {
        m.snapshot()
            .into_iter()
            .find(|(r, _)| r.id == id)
            .map(|(r, _)| r.status)
            .unwrap()
    }

    #[test]
    fn downloads_run_two_at_a_time() {
        let dir = temp_dir("queue");
        let (m, _db) = manager(gated_ani_cli(&dir), Duration::from_secs(60));
        let ids: Vec<String> = (1..=4).map(|ep| start_ep(&m, &dir, ep)).collect();
        wait_for(&m, &ids[1], |r| r.status == DlStatus::Running);
        wait_for(&m, &ids[0], |r| r.status == DlStatus::Running);
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(started_eps(&dir), ["1", "2"]);
        assert_eq!(status_of(&m, &ids[2]), DlStatus::Queued);
        assert_eq!(status_of(&m, &ids[3]), DlStatus::Queued);

        fs::write(dir.join("release"), b"").unwrap();
        for id in &ids {
            assert_eq!(wait_for(&m, id, finished).status, DlStatus::Done);
        }
        assert_eq!(started_eps(&dir), ["1", "2", "3", "4"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cancel_while_queued_never_runs_then_retry_requeues() {
        let dir = temp_dir("queue-cancel");
        let (m, db) = manager(gated_ani_cli(&dir), Duration::from_secs(60));
        let ids: Vec<String> = (1..=3).map(|ep| start_ep(&m, &dir, ep)).collect();
        wait_for(&m, &ids[0], |r| r.status == DlStatus::Running);
        wait_for(&m, &ids[1], |r| r.status == DlStatus::Running);
        m.cancel(&ids[2]);
        assert_eq!(status_of(&m, &ids[2]), DlStatus::Cancelled);
        let saved = db.lock().unwrap().list_downloads().unwrap();
        let saved = saved.iter().find(|r| r.id == ids[2]).unwrap();
        assert_eq!(saved.status, DlStatus::Cancelled);

        fs::write(dir.join("release"), b"").unwrap();
        for id in &ids[..2] {
            assert_eq!(wait_for(&m, id, finished).status, DlStatus::Done);
        }
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(started_eps(&dir), ["1", "2"]);
        assert_eq!(status_of(&m, &ids[2]), DlStatus::Cancelled);

        m.retry(&ids[2]).unwrap();
        assert_eq!(wait_for(&m, &ids[2], finished).status, DlStatus::Done);
        assert_eq!(started_eps(&dir), ["1", "2", "3"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn shutdown_drops_queued_downloads() {
        let dir = temp_dir("queue-shutdown");
        let (m, _db) = manager(gated_ani_cli(&dir), Duration::from_secs(60));
        let ids: Vec<String> = (1..=3).map(|ep| start_ep(&m, &dir, ep)).collect();
        wait_for(&m, &ids[0], |r| r.status == DlStatus::Running);
        wait_for(&m, &ids[1], |r| r.status == DlStatus::Running);
        m.shutdown();
        fs::write(dir.join("release"), b"").unwrap();
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(started_eps(&dir), ["1", "2"]);
        for id in &ids {
            assert_eq!(status_of(&m, id), DlStatus::Failed);
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn download_needs_pick_fails_with_hint() {
        let dir = temp_dir("pick");
        let script = fake_ani_cli(
            &dir.join("bin"),
            r#"printf '["A","B"]' > "$ANI_GUI_MENU_OUT"
printf 'Invalid anime selection\n' >&2
exit 1"#,
        );
        let (m, _db) = manager(script, Duration::from_secs(60));
        let id = m.start(&show(), req(), &dir.join("out")).unwrap();
        let rec = wait_for(&m, &id, finished);
        assert_eq!(rec.status, DlStatus::Failed);
        assert_eq!(
            rec.error.as_deref(),
            Some("Pick the matching title on the show page first")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn download_stall_fails() {
        let dir = temp_dir("stall");
        let script = fake_ani_cli(
            &dir.join("bin"),
            r#"printf '[download]  40.0%% of ~100.00MiB at  1.00MiB/s ETA 01:00\n'
sleep 30"#,
        );
        let (m, _db) = manager(script, Duration::from_secs(1));
        let begun = Instant::now();
        let id = m.start(&show(), req(), &dir.join("out")).unwrap();
        let rec = wait_for(&m, &id, finished);
        assert_eq!(rec.status, DlStatus::Failed);
        assert_eq!(rec.error.as_deref(), Some("stalled"));
        assert_eq!(rec.progress, 40.0);
        assert!(begun.elapsed() < Duration::from_secs(8));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pty_fds_are_cloexec() {
        use nix::fcntl::{FcntlArg, FdFlag, fcntl};
        use std::os::fd::AsFd;

        let (master, slave) = open_pty().unwrap();
        for fd in [master.as_fd(), slave.as_fd()] {
            let flags = FdFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFD).unwrap());
            assert!(flags.contains(FdFlag::FD_CLOEXEC));
        }
        assert!(nix::unistd::isatty(&slave).unwrap());
    }

    #[test]
    fn concurrent_downloads_all_finish() {
        let dir = temp_dir("concurrent");
        let script = fake_ani_cli(
            &dir.join("bin"),
            r#"while [ $# -gt 0 ]; do [ "$1" = -e ] && ep=$2; shift; done
case "$ep" in
  1|3|5) ;;
  *) for p in 10 20 30 40 50 60 70 80; do
       printf '[download]  %s.0%% of ~10.00MiB at  1.00MiB/s ETA 00:01\n' "$p"
       sleep 0.25
     done ;;
esac
echo x > "$ANI_CLI_DOWNLOAD_DIR/Frieren Episode $ep.mp4""#,
        );
        let (m, _db) = manager(script, Duration::from_secs(1));
        let ids: Vec<String> = (1..=6)
            .map(|ep| {
                let req = EpisodeRequest {
                    ep: ep.to_string(),
                    ..req()
                };
                m.start(&show(), req, &dir.join(format!("out{ep}")))
                    .unwrap()
            })
            .collect();
        for (ep, id) in (1..=6).zip(&ids) {
            let rec = wait_for(&m, id, finished);
            assert_eq!(rec.status, DlStatus::Done, "episode {ep}: {rec:?}");
            assert_eq!(
                rec.path.as_deref(),
                dir.join(format!("out{ep}/Frieren Episode {ep}.mp4"))
                    .to_str()
            );
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn exited_download_does_not_wait_for_lingering_pty_holder() {
        let dir = temp_dir("linger");
        // The background sleep keeps the pty open long after ani-cli exits 0.
        let script = fake_ani_cli(
            &dir.join("bin"),
            r#"echo x > "$ANI_CLI_DOWNLOAD_DIR/Frieren Episode 3.mp4"
sleep 8 &
exit 0"#,
        );
        let (m, _db) = manager(script, Duration::from_secs(1));
        let begun = Instant::now();
        let id = m.start(&show(), req(), &dir.join("out")).unwrap();
        let rec = wait_for(&m, &id, finished);
        assert_eq!(rec.status, DlStatus::Done, "{rec:?}");
        assert!(
            begun.elapsed() < Duration::from_secs(5),
            "{:?}",
            begun.elapsed()
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn shutdown_stops_active_downloads() {
        let dir = temp_dir("shutdown");
        let script = fake_ani_cli(
            &dir.join("bin"),
            r#"trap '' TERM
sleep 30 &
echo $! > "$ANI_CLI_DOWNLOAD_DIR/child.pid"
printf '[download]   5.0%% of ~100.00MiB at  1.00MiB/s ETA 09:00\n'
wait"#,
        );
        let (m, db) = manager(script, Duration::from_secs(60));
        let out = dir.join("out");
        let id = m.start(&show(), req(), &out).unwrap();
        wait_for(&m, &id, |r| {
            r.status == DlStatus::Running && r.progress == 5.0
        });

        let begun = Instant::now();
        m.shutdown();
        assert!(
            begun.elapsed() < Duration::from_secs(4),
            "{:?}",
            begun.elapsed()
        );
        let child: i32 = fs::read_to_string(out.join("child.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while nix::sys::signal::kill(nix::unistd::Pid::from_raw(child), None).is_ok() {
            assert!(
                Instant::now() < deadline,
                "grandchild {child} survived shutdown"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // The worker winding down afterwards must not overwrite the record.
        std::thread::sleep(Duration::from_millis(500));
        let saved = db.lock().unwrap().list_downloads().unwrap().remove(0);
        assert_eq!(saved.status, DlStatus::Failed);
        assert_eq!(saved.error.as_deref(), Some("app closed"));
        assert_eq!(m.snapshot()[0].0, saved);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn interrupted_downloads_fail_on_load() {
        let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
        let rec = DownloadRecord {
            id: "old".into(),
            show_id: "s".into(),
            title: "T".into(),
            ep: "1".into(),
            mode: Mode::Sub,
            quality: "best".into(),
            path: None,
            status: DlStatus::Running,
            progress: 12.0,
            error: None,
            created_at: 1,
        };
        db.lock().unwrap().upsert_download(&rec).unwrap();
        let m = DownloadManager::new(db.clone());
        let (loaded, progress) = m.snapshot().remove(0);
        assert_eq!(loaded.status, DlStatus::Failed);
        assert_eq!(loaded.error.as_deref(), Some("interrupted"));
        assert_eq!(progress, None);
        assert_eq!(
            db.lock().unwrap().list_downloads().unwrap()[0].status,
            DlStatus::Failed
        );
        assert!(
            m.retry("old").is_err(),
            "no request is known for a past session"
        );
    }
}
