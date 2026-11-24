//! Updates from GitHub Releases.
//!
//! Trust comes from an Ed25519 key, not from GitHub. Anyone holding a token
//! with Contents:write can upload release assets, a matching checksum file
//! included, so an archive is accepted only when its SHA-256 appears in a
//! `SHA256SUMS` signed by the key embedded below. The private key lives only
//! with the maintainer and in the release workflow's secret (see
//! `docs/releasing.md`).
//!
//! An install extracts `bin/ani-gui` next to the installed binary and
//! renames it over the old one. The running process keeps its old inode, so
//! the new version starts the next time the app does. Dev builds, binaries
//! the user can't write and root are never replaced: they are only told
//! that an update exists.

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use ed25519_dalek::{Signature, VerifyingKey};
use gpui::{App, AppContext, Entity, Global};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::app::AppState;
use crate::paths;
use crate::ui::components::toast;

const REPO: &str = "SweetTooth05/ani-gui";
const API_LATEST: &str = "https://api.github.com/repos/SweetTooth05/ani-gui/releases/latest";
const DOWNLOAD_PREFIX: &str = "https://github.com/SweetTooth05/ani-gui/releases/download/";
/// Generic on purpose: nothing about the user goes to GitHub.
const USER_AGENT: &str = "ani-gui-updater";

/// The version this binary was built as.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// The Rust target triple this binary was built for (see `build.rs`).
const TARGET: &str = env!("ANI_GUI_TARGET");

/// The release signing keys a `SHA256SUMS` signature may come from: raw
/// Ed25519 public keys, in the same order as the PEM blocks in
/// `docs/release-signing-key.pub.pem` (a test keeps them equal). Normally
/// one; during a key rotation the old and the new (docs/releasing.md).
const RELEASE_PUBLIC_KEYS: &[[u8; 32]] = &[[
    0x6f, 0x21, 0x16, 0xd0, 0x7d, 0x8a, 0x60, 0x2d, 0x8e, 0xee, 0x23, 0x00, 0xe6, 0xf8, 0x9f, 0x8f,
    0xc6, 0x23, 0x7f, 0x21, 0x54, 0xf0, 0x6f, 0x03, 0xdf, 0xa9, 0xd4, 0xe6, 0x13, 0x9b, 0x8e, 0x37,
]];

const SUMS: &str = "SHA256SUMS";
const SIG: &str = "SHA256SUMS.sig";

const CHECK_EVERY_SECS: i64 = 6 * 60 * 60;
/// Lets startup finish before the network is touched.
const STARTUP_DELAY: Duration = Duration::from_secs(5);
/// How often a long-running app looks whether a check is due.
const POLL_EVERY: Duration = Duration::from_secs(15 * 60);

const MAX_JSON: u64 = 1 << 20;
const MAX_SUMS: u64 = 64 << 10;
const MAX_SIG: u64 = 1 << 10;
const MAX_ARCHIVE: u64 = 256 << 20;
const MAX_BINARY: u64 = 512 << 20;

/// A plain `MAJOR.MINOR.PATCH` version. Pre-release and build suffixes are
/// not accepted, so a tag like `v2.0.0-rc1` never counts as newer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    /// Parses `1.2.3` or `v1.2.3`.
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.strip_prefix('v').unwrap_or(s);
        let parts: Vec<&str> = s.split('.').collect();
        let [major, minor, patch] = parts.as_slice() else {
            return None;
        };
        let num = |p: &str| -> Option<u64> {
            let digits = !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
            let leading_zero = p.len() > 1 && p.starts_with('0');
            if !digits || leading_zero {
                return None;
            }
            p.parse().ok()
        };
        Some(Version {
            major: num(major)?,
            minor: num(minor)?,
            patch: num(patch)?,
        })
    }

    pub fn current() -> Version {
        Version::parse(CURRENT).expect("the Cargo version is MAJOR.MINOR.PATCH")
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Where releases come from. Only tests point it anywhere but GitHub.
#[derive(Clone, Debug)]
pub struct Source {
    api_url: String,
    /// Every asset URL must be exactly `{prefix}v{version}/{name}`.
    download_prefix: String,
    https_only: bool,
}

impl Source {
    pub fn github() -> Source {
        Source {
            api_url: API_LATEST.into(),
            download_prefix: DOWNLOAD_PREFIX.into(),
            https_only: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: Version,
    assets: Vec<Asset>,
}

impl Release {
    /// Built from the version rather than taken from the API, so the link
    /// the user opens always points at this repository.
    pub fn page_url(&self) -> String {
        format!("https://github.com/{REPO}/releases/tag/v{}", self.version)
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Asset {
    name: String,
    url: String,
    size: u64,
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

/// The published release in GitHub's `releases/latest` JSON; None for
/// drafts and prereleases.
fn parse_release(json: &[u8]) -> Result<Option<Release>> {
    let api: ApiRelease = serde_json::from_slice(json).context("decoding the release")?;
    if api.draft || api.prerelease {
        return Ok(None);
    }
    let version = api
        .tag_name
        .starts_with('v')
        .then(|| Version::parse(&api.tag_name))
        .flatten()
        .with_context(|| format!("the latest release tag {:?} isn't vX.Y.Z", api.tag_name))?;
    let assets = api
        .assets
        .into_iter()
        .map(|a| Asset {
            name: a.name,
            url: a.browser_download_url,
            size: a.size,
        })
        .collect();
    Ok(Some(Release { version, assets }))
}

fn archive_stem(version: Version, target: &str) -> String {
    format!("ani-gui-{version}-{target}")
}

fn archive_name(version: Version, target: &str) -> String {
    format!("{}.tar.gz", archive_stem(version, target))
}

struct Picked<'a> {
    archive: &'a Asset,
    sums: &'a Asset,
    sig: &'a Asset,
}

/// The archive for `target` and the signed checksums, each present once
/// and served from this release's download folder.
fn pick_assets<'a>(release: &'a Release, target: &str, prefix: &str) -> Result<Picked<'a>> {
    let find = |name: &str| -> Result<&'a Asset> {
        let mut found = release.assets.iter().filter(|a| a.name == name);
        let asset = found
            .next()
            .with_context(|| format!("release v{} has no {name}", release.version))?;
        ensure!(
            found.next().is_none(),
            "release v{} lists {name} twice",
            release.version
        );
        let expected = format!("{prefix}v{}/{name}", release.version);
        ensure!(
            asset.url == expected,
            "{name} is served from an unexpected address: {}",
            asset.url
        );
        Ok(asset)
    };
    Ok(Picked {
        archive: find(&archive_name(release.version, target))?,
        sums: find(SUMS)?,
        sig: find(SIG)?,
    })
}

/// Checks the raw 64-byte Ed25519 signature `openssl pkeyutl -sign -rawin`
/// writes for `SHA256SUMS` against each of `keys`.
fn verify_sums(sums: &[u8], sig: &[u8], keys: &[[u8; 32]]) -> Result<()> {
    let sig: [u8; 64] = sig
        .try_into()
        .map_err(|_| anyhow!("{SIG} is {} bytes, not 64", sig.len()))?;
    let sig = Signature::from_bytes(&sig);
    for key in keys {
        let key = VerifyingKey::from_bytes(key).context("a release key is invalid")?;
        if key.verify_strict(sums, &sig).is_ok() {
            return Ok(());
        }
    }
    bail!("{SUMS} isn't signed by the ani-gui release key")
}

/// The SHA-256 that a verified `SHA256SUMS` (`sha256sum` output) lists for
/// `name`, which must appear exactly once.
fn expected_hash(sums: &[u8], name: &str) -> Result<[u8; 32]> {
    let text = std::str::from_utf8(sums).with_context(|| format!("{SUMS} isn't text"))?;
    let mut found = None;
    for line in text.lines() {
        let (Some(hex), Some(rest)) = (line.get(..64), line.get(64..)) else {
            continue;
        };
        let file = rest.strip_prefix("  ").or_else(|| rest.strip_prefix(" *"));
        if file != Some(name) {
            continue;
        }
        ensure!(found.is_none(), "{SUMS} lists {name} twice");
        found = Some(parse_hex32(hex).with_context(|| format!("bad hash for {name} in {SUMS}"))?);
    }
    found.with_context(|| format!("{SUMS} has no entry for {name}"))
}

fn parse_hex32(hex: &str) -> Option<[u8; 32]> {
    let digit = |b: u8| (b as char).to_digit(16);
    let bytes = hex.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in bytes.chunks(2).enumerate() {
        out[i] = (digit(pair[0])? * 16 + digit(pair[1])?) as u8;
    }
    Some(out)
}

fn agent(https_only: bool) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_global(Some(Duration::from_secs(10 * 60)))
        .https_only(https_only)
        .user_agent(USER_AGENT)
        .build()
        .into()
}

fn get_small(agent: &ureq::Agent, url: &str, limit: u64) -> Result<Vec<u8>> {
    agent
        .get(url)
        .call()
        .with_context(|| format!("GET {url}"))?
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .with_context(|| format!("reading {url}"))
}

/// Streams `url` into `out` (at most `limit` bytes) and returns its SHA-256.
fn download(agent: &ureq::Agent, url: &str, out: &mut File, limit: u64) -> Result<[u8; 32]> {
    let mut response = agent
        .get(url)
        .call()
        .with_context(|| format!("GET {url}"))?;
    let mut body = response.body_mut().with_config().limit(limit).reader();
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let n = match body.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err).with_context(|| format!("downloading {url}")),
        };
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n]).context("saving the download")?;
    }
    out.sync_all().context("saving the download")?;
    Ok(hasher.finalize().into())
}

/// The newest published release if it is newer than `current`.
pub fn latest_newer(source: &Source, current: Version) -> Result<Option<Release>> {
    let agent = agent(source.https_only);
    let response = agent
        .get(&source.api_url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call();
    let mut response = match response {
        // No release has been published yet.
        Err(ureq::Error::StatusCode(404)) => return Ok(None),
        other => other.context("asking GitHub for the latest release")?,
    };
    let json = response
        .body_mut()
        .with_config()
        .limit(MAX_JSON)
        .read_to_vec()
        .context("reading the latest release")?;
    Ok(parse_release(&json)?.filter(|r| r.version > current))
}

/// Downloads `release` for `target`, verifies it against `key`, and swaps
/// its `bin/ani-gui` in at `exe`. Any failure leaves `exe` untouched.
fn install(
    source: &Source,
    release: &Release,
    target: &str,
    exe: &Path,
    keys: &[[u8; 32]],
) -> Result<()> {
    let picked = pick_assets(release, target, &source.download_prefix)?;
    ensure!(
        picked.archive.size <= MAX_ARCHIVE,
        "{} is too large ({} bytes)",
        picked.archive.name,
        picked.archive.size
    );
    let agent = agent(source.https_only);
    let sums = get_small(&agent, &picked.sums.url, MAX_SUMS)?;
    let sig = get_small(&agent, &picked.sig.url, MAX_SIG)?;
    // The signature first: nothing in SHA256SUMS counts until it verifies.
    verify_sums(&sums, &sig, keys)?;
    let expected = expected_hash(&sums, &picked.archive.name)?;
    let dir = exe.parent().context("the install path has no folder")?;
    let mut archive = TempFile::new(dir, "download")?;
    let actual = download(&agent, &picked.archive.url, &mut archive.file, MAX_ARCHIVE)?;
    ensure!(
        actual == expected,
        "{} doesn't match its signed checksum",
        picked.archive.name
    );
    archive.file.seek(SeekFrom::Start(0))?;
    replace_binary(&archive.file, &archive_stem(release.version, target), exe)
}

/// Writes the archive's `<top>/bin/ani-gui` into `out`. Only that one
/// regular file is read; any entry whose path is absolute or climbs out
/// with `..` rejects the whole archive.
fn extract_binary(archive: impl Read, top: &str, out: &mut impl Write, limit: u64) -> Result<()> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    let mut found = false;
    for entry in tar.entries().context("reading the archive")? {
        let mut entry = entry.context("reading the archive")?;
        let path = entry.path().context("reading the archive")?.into_owned();
        let mut parts = Vec::new();
        for part in path.components() {
            match part {
                Component::Normal(p) => parts.push(p.to_owned()),
                Component::CurDir => {}
                _ => bail!("the archive has an unsafe path: {}", path.display()),
            }
        }
        if parts != [top, "bin", "ani-gui"] {
            continue;
        }
        ensure!(!found, "the archive has bin/ani-gui twice");
        ensure!(
            entry.header().entry_type() == tar::EntryType::Regular,
            "bin/ani-gui in the archive isn't a regular file"
        );
        ensure!(
            entry.size() <= limit,
            "bin/ani-gui in the archive is too large"
        );
        let copied =
            io::copy(&mut (&mut entry).take(limit + 1), out).context("extracting bin/ani-gui")?;
        ensure!(copied <= limit, "bin/ani-gui in the archive is too large");
        found = true;
    }
    ensure!(found, "the archive has no {top}/bin/ani-gui");
    Ok(())
}

/// Whether `magic` starts an executable for this OS.
fn plausible_executable(magic: &[u8; 4]) -> bool {
    if cfg!(target_os = "macos") {
        matches!(
            magic,
            [0xcf, 0xfa, 0xed, 0xfe] | [0xce, 0xfa, 0xed, 0xfe] | [0xca, 0xfe, 0xba, 0xbe]
        )
    } else {
        magic == b"\x7fELF"
    }
}

/// Extracts the new binary beside `exe`, checks it, and renames it over
/// `exe` in one step.
fn replace_binary(archive: impl Read, top: &str, exe: &Path) -> Result<()> {
    let dir = exe.parent().context("the install path has no folder")?;
    let mut new = TempFile::new(dir, "new")?;
    extract_binary(archive, top, &mut new.file, MAX_BINARY)?;
    let mut magic = [0u8; 4];
    new.file
        .read_exact_at(&mut magic, 0)
        .context("the new binary is empty")?;
    ensure!(
        plausible_executable(&magic),
        "bin/ani-gui in the archive isn't an executable for this system"
    );
    new.file.set_permissions(Permissions::from_mode(0o755))?;
    new.file.sync_all().context("saving the new binary")?;
    fs::rename(&new.path, exe).with_context(|| format!("replacing {}", exe.display()))?;
    new.keep = true;
    // Makes the rename itself durable; the swap has happened either way.
    if let Err(err) = File::open(dir).and_then(|d| d.sync_all()) {
        eprintln!("ani-gui: update: syncing {}: {err}", dir.display());
    }
    Ok(())
}

/// A file created next to the installed binary (so the final rename stays
/// on one filesystem), removed again unless kept.
struct TempFile {
    path: PathBuf,
    file: File,
    keep: bool,
}

impl TempFile {
    fn new(dir: &Path, what: &str) -> Result<TempFile> {
        static COUNT: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let n = COUNT.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(
            ".ani-gui-{what}-{}-{nanos}-{n}",
            std::process::id()
        ));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        Ok(TempFile {
            path,
            file,
            keep: false,
        })
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Why ani-gui only reports updates instead of installing them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotEligible {
    UnknownPath,
    DevBuild,
    Root,
    NotOwned,
    NotWritable,
}

impl NotEligible {
    pub fn explain(self) -> &'static str {
        match self {
            NotEligible::UnknownPath => "ani-gui couldn't tell where it is installed.",
            NotEligible::DevBuild => "This is a development build, so it never replaces itself.",
            NotEligible::Root => "ani-gui is running as root, so it never replaces itself.",
            NotEligible::NotOwned => "The ani-gui binary belongs to another user.",
            NotEligible::NotWritable => {
                "ani-gui is installed in a folder you can't write to, such as a system install."
            }
        }
    }
}

/// `exe` if ani-gui may replace it: a release build outside cargo's
/// `target/`, run by the binary's owner, in a folder that user can write.
fn self_update_path(exe: &Path, dev_build: bool) -> Result<PathBuf, NotEligible> {
    if dev_build || in_cargo_target(exe) {
        return Err(NotEligible::DevBuild);
    }
    let euid = euid();
    if euid == 0 {
        return Err(NotEligible::Root);
    }
    let meta = fs::metadata(exe).map_err(|_| NotEligible::UnknownPath)?;
    if meta.uid() != euid {
        return Err(NotEligible::NotOwned);
    }
    let dir = exe.parent().ok_or(NotEligible::UnknownPath)?;
    nix::unistd::access(dir, nix::unistd::AccessFlags::W_OK)
        .map_err(|_| NotEligible::NotWritable)?;
    Ok(exe.to_path_buf())
}

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { nix::libc::geteuid() }
}

/// Whether `exe` sits in cargo's output: `target/{debug,release}/` or
/// `target/<triple>/{debug,release}/`.
fn in_cargo_target(exe: &Path) -> bool {
    let parts: Vec<_> = exe.components().map(|c| c.as_os_str()).collect();
    parts.iter().enumerate().any(|(i, part)| {
        let profile = |j: usize| {
            parts
                .get(j)
                .is_some_and(|p| *p == "debug" || *p == "release")
        };
        *part == "target" && (profile(i + 1) || profile(i + 2))
    })
}

/// The installed binary's path, read once: after an update replaces it,
/// `/proc/self/exe` reads "<path> (deleted)".
fn exe_path() -> Option<PathBuf> {
    static EXE: OnceLock<Option<PathBuf>> = OnceLock::new();
    EXE.get_or_init(|| {
        let exe = std::env::current_exe().ok()?;
        let text = exe.to_str()?;
        Some(PathBuf::from(
            text.strip_suffix(" (deleted)").unwrap_or(text),
        ))
    })
    .clone()
}

/// What the last check found, kept across restarts.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Saved {
    /// Unix seconds.
    pub last_check: Option<i64>,
    pub result: Option<String>,
    /// A version already swapped in, so it isn't downloaded again before
    /// the restart that starts it.
    pub installed: Option<String>,
}

fn state_path() -> PathBuf {
    paths::data_dir().join("update-state.json")
}

fn load_saved(path: &Path) -> Saved {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn store_saved(path: &Path, saved: &Saved) -> Result<()> {
    let dir = path.parent().context("state path has no folder")?;
    fs::create_dir_all(dir)?;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&tmp, serde_json::to_vec_pretty(saved)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Whether a scheduled check should run: never checked, six hours passed,
/// or the clock went backwards.
fn check_due(last: Option<i64>, now: i64) -> bool {
    last.is_none_or(|t| now < t || now - t >= CHECK_EVERY_SECS)
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// "3 minutes ago" and the like, for the last-check line.
pub fn ago(then: i64, now: i64) -> String {
    let secs = (now - then).max(0);
    let (n, unit) = match secs {
        0..60 => return "just now".into(),
        60..3600 => (secs / 60, "minute"),
        3600..86400 => (secs / 3600, "hour"),
        _ => (secs / 86400, "day"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

// ---------------------------------------------------------------------------
// App glue: one `Updates` model the Settings page watches.

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Idle,
    Checking,
    UpToDate,
    /// Newer and not installed: notify-only installs, or automatic installs
    /// turned off.
    Available(Release),
    Installing(Version),
    /// Swapped in; it runs after a restart.
    Installed(Version),
    Failed {
        error: String,
        release: Option<Release>,
    },
}

pub struct Updates {
    pub status: Status,
    pub saved: Saved,
    /// Where this install can update itself, or why it can't.
    pub install_path: Result<PathBuf, NotEligible>,
}

impl Updates {
    pub fn busy(&self) -> bool {
        matches!(self.status, Status::Checking | Status::Installing(_))
    }
}

struct UpdatesHandle(Entity<Updates>);

impl Global for UpdatesHandle {}

pub fn model(cx: &App) -> Option<Entity<Updates>> {
    cx.try_global::<UpdatesHandle>().map(|h| h.0.clone())
}

/// Starts the updater: loads the saved state, then checks a few seconds
/// after launch and every six hours while the app runs.
pub fn init(cx: &mut App) {
    let exe = exe_path();
    let updates = cx.new(|_| Updates {
        status: Status::Idle,
        saved: Saved::default(),
        install_path: Err(NotEligible::UnknownPath),
    });
    cx.set_global(UpdatesHandle(updates.clone()));
    cx.spawn(async move |cx| {
        let (saved, install_path) = cx
            .background_spawn(async move {
                let install_path = exe.map_or(Err(NotEligible::UnknownPath), |exe| {
                    self_update_path(&exe, cfg!(debug_assertions))
                });
                (load_saved(&state_path()), install_path)
            })
            .await;
        if let Err(why) = install_path {
            eprintln!("ani-gui: update: notify only: {}", why.explain());
        }
        let loaded = updates.update(cx, |u, cx| {
            u.saved = saved;
            u.install_path = install_path;
            cx.notify();
        });
        if loaded.is_err() {
            return;
        }
        cx.background_executor().timer(STARTUP_DELAY).await;
        loop {
            let Ok(due) = updates.read_with(cx, |u, _| check_due(u.saved.last_check, now())) else {
                return;
            };
            if due && cx.update(|cx| check(cx, false)).is_err() {
                return;
            }
            cx.background_executor().timer(POLL_EVERY).await;
        }
    })
    .detach();
}

/// The Settings page's "Check now".
pub fn check_now(cx: &mut App) {
    check(cx, true);
}

fn check(cx: &mut App, manual: bool) {
    let Some(updates) = model(cx) else {
        return;
    };
    let u = updates.read(cx);
    if u.busy() {
        return;
    }
    let auto = cx
        .try_global::<AppState>()
        .is_none_or(|s| s.settings.auto_update);
    let install_to = u.install_path.clone().ok().filter(|_| auto);
    let installed = u.saved.installed.as_deref().and_then(Version::parse);
    updates.update(cx, |u, cx| {
        u.status = Status::Checking;
        cx.notify();
    });
    cx.spawn(async move |cx| {
        let found = cx
            .background_spawn(async { latest_newer(&Source::github(), Version::current()) })
            .await;
        cx.update(|cx| {
            let status = match found {
                Err(err) => Status::Failed {
                    error: format!("{err:#}"),
                    release: None,
                },
                Ok(None) => Status::UpToDate,
                Ok(Some(release)) if installed == Some(release.version) => {
                    Status::Installed(release.version)
                }
                Ok(Some(release)) => match install_to {
                    Some(exe) => return start_install(cx, release, exe, manual, true),
                    None => Status::Available(release),
                },
            };
            finish(cx, status, manual, true);
        })
        .ok();
    })
    .detach();
}

/// The Settings page's "Install", for an update found while automatic
/// installs were off.
pub fn install_now(cx: &mut App) {
    let Some(updates) = model(cx) else {
        return;
    };
    let u = updates.read(cx);
    let (Status::Available(release)
    | Status::Failed {
        release: Some(release),
        ..
    }) = &u.status
    else {
        return;
    };
    let Ok(exe) = u.install_path.clone() else {
        return;
    };
    let release = release.clone();
    start_install(cx, release, exe, true, false);
}

fn start_install(cx: &mut App, release: Release, exe: PathBuf, manual: bool, checked: bool) {
    let Some(updates) = model(cx) else {
        return;
    };
    let version = release.version;
    updates.update(cx, |u, cx| {
        u.status = Status::Installing(version);
        cx.notify();
    });
    cx.spawn(async move |cx| {
        let installed = {
            let release = release.clone();
            cx.background_spawn(async move {
                install(
                    &Source::github(),
                    &release,
                    TARGET,
                    &exe,
                    RELEASE_PUBLIC_KEYS,
                )
            })
            .await
        };
        let status = match installed {
            Ok(()) => Status::Installed(version),
            Err(err) => Status::Failed {
                error: format!("Couldn't install v{version}: {err:#}"),
                release: Some(release),
            },
        };
        cx.update(|cx| finish(cx, status, manual, checked)).ok();
    })
    .detach();
}

/// Records what a check or install ended with and tells the user.
fn finish(cx: &mut App, status: Status, manual: bool, checked: bool) {
    let Some(updates) = model(cx) else {
        return;
    };
    let result = match &status {
        Status::UpToDate => "Up to date".to_string(),
        Status::Available(r) => format!("v{} available", r.version),
        Status::Installed(v) => format!("Updated to v{v}"),
        Status::Failed { error, .. } => format!("Failed: {error}"),
        Status::Idle | Status::Checking | Status::Installing(_) => return,
    };
    let previous = updates.read(cx).saved.result.clone();
    match &status {
        Status::Installed(v) if previous.as_deref() != Some(result.as_str()) || manual => {
            toast::info(cx, format!("Updated to v{v} — restart to use it"));
        }
        Status::Available(r) if manual || previous.as_deref() != Some(result.as_str()) => {
            toast::info(
                cx,
                format!("ani-gui v{} is available — see Settings", r.version),
            );
        }
        Status::Failed { error, .. } => {
            eprintln!("ani-gui: update: {error}");
            if manual {
                toast::error(cx, "Update failed — see Settings");
            }
        }
        Status::UpToDate if manual => toast::info(cx, "ani-gui is up to date"),
        _ => {}
    }
    let saved = updates.update(cx, |u, cx| {
        if checked {
            u.saved.last_check = Some(now());
        }
        if let Status::Installed(v) = &status {
            u.saved.installed = Some(v.to_string());
        }
        u.saved.result = Some(result);
        u.status = status;
        cx.notify();
        u.saved.clone()
    });
    cx.background_spawn(async move {
        if let Err(err) = store_saved(&state_path(), &saved) {
            eprintln!("ani-gui: update: saving state: {err:#}");
        }
    })
    .detach();
}

/// Starts the freshly installed binary and quits this one.
pub fn restart(cx: &mut App) {
    let Some(exe) = exe_path() else {
        toast::error(cx, "Couldn't find ani-gui to restart it");
        return;
    };
    match Command::new(&exe).spawn() {
        Ok(_) => cx.quit(),
        Err(err) => toast::error(cx, format!("Couldn't restart ani-gui: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use std::io::BufRead;
    use std::net::TcpListener;

    const T: &str = "x86_64-unknown-linux-gnu";

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ani-gui-updater-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sha256(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// A gzipped tar holding `entries` as regular files.
    fn tarball(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut tar = tar::Builder::new(gz);
        for (path, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_entry_type(tar::EntryType::Regular);
            tar.append_data(&mut header, path, *data).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    /// A tarball with one entry whose raw name bypasses the tar crate's
    /// own path checks, as a hostile archive would.
    fn raw_tarball(name: &str, kind: tar::EntryType, data: &[u8]) -> Vec<u8> {
        let mut header = tar::Header::new_old();
        let raw = &mut header.as_old_mut().name;
        raw[..name.len()].copy_from_slice(name.as_bytes());
        header.set_size(data.len() as u64);
        header.set_mode(0o755);
        header.set_entry_type(kind);
        if kind == tar::EntryType::Symlink {
            header.set_link_name("/bin/sh").unwrap();
        }
        header.set_cksum();
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut tar = tar::Builder::new(gz);
        tar.append(&header, data).unwrap();
        tar.into_inner().unwrap().finish().unwrap()
    }

    const ELF: &[u8] = b"\x7fELF new ani-gui";

    #[test]
    fn versions_parse_and_compare() {
        assert_eq!(Version::parse("v1.2.3"), Version::parse("1.2.3"));
        assert!(v("1.0.10") > v("1.0.9"));
        assert!(v("2.0.0") > v("1.99.99"));
        assert!(v("1.1.0") > v("1.0.5"));
        assert_eq!(v("10.0.0").to_string(), "10.0.0");
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "v1.2.x",
            "1.2.3-rc1",
            "1.2.3+b",
            "01.2.3",
            "1..3",
            " 1.2.3",
            "vv1.2.3",
            "-1.2.3",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
        assert_eq!(Version::current().to_string(), CURRENT);
    }

    fn release_json(tag: &str, draft: bool, prerelease: bool, assets: &[(&str, &str)]) -> Vec<u8> {
        let assets: Vec<_> = assets
            .iter()
            .map(|(name, url)| serde_json::json!({"name": name, "browser_download_url": url, "size": 10}))
            .collect();
        serde_json::to_vec(&serde_json::json!({
            "tag_name": tag, "draft": draft, "prerelease": prerelease,
            "html_url": "https://evil.example/", "assets": assets,
        }))
        .unwrap()
    }

    #[test]
    fn releases_skip_drafts_prereleases_and_odd_tags() {
        let r = parse_release(&release_json("v1.2.0", false, false, &[]))
            .unwrap()
            .unwrap();
        assert_eq!(r.version, v("1.2.0"));
        // The page link ignores html_url from the response.
        assert_eq!(
            r.page_url(),
            "https://github.com/SweetTooth05/ani-gui/releases/tag/v1.2.0"
        );
        assert_eq!(
            parse_release(&release_json("v1.2.0", true, false, &[])).unwrap(),
            None
        );
        assert_eq!(
            parse_release(&release_json("v1.2.0", false, true, &[])).unwrap(),
            None
        );
        assert!(parse_release(&release_json("1.2.0", false, false, &[])).is_err());
        assert!(parse_release(&release_json("v1.2.0-beta", false, false, &[])).is_err());
        assert!(parse_release(b"not json").is_err());
    }

    #[test]
    fn assets_are_picked_for_the_target_from_the_release_folder() {
        let p = DOWNLOAD_PREFIX;
        let url = |name: &str| format!("{p}v1.1.0/{name}");
        let names = [
            "ani-gui-1.1.0-x86_64-unknown-linux-gnu.tar.gz",
            "ani-gui-1.1.0-aarch64-apple-darwin.tar.gz",
            SUMS,
            SIG,
        ];
        let assets: Vec<_> = names.iter().map(|n| (*n, url(n))).collect();
        let pairs: Vec<(&str, &str)> = assets.iter().map(|(n, u)| (*n, u.as_str())).collect();
        let release = parse_release(&release_json("v1.1.0", false, false, &pairs))
            .unwrap()
            .unwrap();

        let picked = pick_assets(&release, T, p).unwrap();
        assert_eq!(picked.archive.name, names[0]);
        assert_eq!(picked.sums.name, SUMS);
        assert_eq!(picked.sig.name, SIG);
        let mac = pick_assets(&release, "aarch64-apple-darwin", p).unwrap();
        assert_eq!(mac.archive.name, names[1]);
        assert!(pick_assets(&release, "riscv64gc-unknown-linux-gnu", p).is_err());

        // An asset served from anywhere else is refused.
        let mut moved = release.clone();
        moved.assets[0].url = "https://evil.example/ani-gui.tar.gz".into();
        assert!(pick_assets(&moved, T, p).is_err());
        // So is a duplicate name.
        let mut doubled = release.clone();
        doubled.assets.push(doubled.assets[2].clone());
        assert!(pick_assets(&doubled, T, p).is_err());
        // And a release without its signature.
        let mut unsigned = release;
        unsigned.assets.retain(|a| a.name != SIG);
        assert!(pick_assets(&unsigned, T, p).is_err());
    }

    fn test_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    #[test]
    fn signature_and_hash_pipeline() {
        let key = test_key(7);
        let public = key.verifying_key().to_bytes();
        let archive = b"archive bytes";
        let name = "ani-gui-1.1.0-x86_64-unknown-linux-gnu.tar.gz";
        let sums = format!(
            "{}  {name}\n{}  other.tar.gz\n",
            sha256(archive),
            sha256(b"x")
        );
        let sig = key.sign(sums.as_bytes()).to_bytes();

        // Valid: the signature verifies and the archive matches.
        verify_sums(sums.as_bytes(), &sig, &[public]).unwrap();
        let want = expected_hash(sums.as_bytes(), name).unwrap();
        assert_eq!(want, <[u8; 32]>::from(Sha256::digest(archive)));

        // Tampered SHA256SUMS: the attacker's archive hash swapped in.
        let tampered = sums.replace(&sha256(archive), &sha256(b"evil"));
        assert_ne!(tampered, sums);
        assert!(verify_sums(tampered.as_bytes(), &sig, &[public]).is_err());
        // Or one appended line.
        let appended = format!("{sums}{}  evil.tar.gz\n", sha256(b"evil"));
        assert!(verify_sums(appended.as_bytes(), &sig, &[public]).is_err());
        // Wrong key.
        let other = test_key(8).verifying_key().to_bytes();
        assert!(verify_sums(sums.as_bytes(), &sig, &[other]).is_err());
        // During a rotation either listed key is accepted.
        verify_sums(sums.as_bytes(), &sig, &[other, public]).unwrap();
        assert!(verify_sums(sums.as_bytes(), &sig, &[]).is_err());
        // Truncated or empty signature.
        assert!(verify_sums(sums.as_bytes(), &sig[..63], &[public]).is_err());
        assert!(verify_sums(sums.as_bytes(), b"", &[public]).is_err());
        // A tampered archive is covered end to end in
        // `end_to_end_tampered_archive_is_refused`.
    }

    #[test]
    fn sums_parsing_is_strict() {
        let h = sha256(b"a");
        assert!(expected_hash(format!("{h}  a.tar.gz\n").as_bytes(), "a.tar.gz").is_ok());
        assert!(expected_hash(format!("{h} *a.tar.gz\n").as_bytes(), "a.tar.gz").is_ok());
        // Different name, prefix or suffix of the name, duplicates, bad hex.
        assert!(expected_hash(format!("{h}  b.tar.gz\n").as_bytes(), "a.tar.gz").is_err());
        assert!(expected_hash(format!("{h}  a.tar.gz.old\n").as_bytes(), "a.tar.gz").is_err());
        assert!(expected_hash(format!("{h}  x/a.tar.gz\n").as_bytes(), "a.tar.gz").is_err());
        let twice = format!("{h}  a.tar.gz\n{h}  a.tar.gz\n");
        assert!(expected_hash(twice.as_bytes(), "a.tar.gz").is_err());
        let bad = format!("{}  a.tar.gz\n", "g".repeat(64));
        assert!(expected_hash(bad.as_bytes(), "a.tar.gz").is_err());
        assert!(expected_hash(b"\xff\xfe", "a.tar.gz").is_err());
    }

    /// The release workflow signs with `openssl pkeyutl -sign -rawin`; this
    /// proves the app accepts exactly that format (skipped without openssl).
    #[test]
    fn openssl_signatures_verify() {
        if Command::new("openssl").arg("version").output().is_err() {
            eprintln!("openssl not found; skipping");
            return;
        }
        let dir = temp_dir("openssl");
        let key = dir.join("key.pem");
        let sums = dir.join("SHA256SUMS");
        let sig = dir.join("SHA256SUMS.sig");
        let pubder = dir.join("pub.der");
        fs::write(&sums, format!("{}  a.tar.gz\n", sha256(b"a"))).unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("openssl").args(args).output().unwrap();
            assert!(out.status.success(), "openssl {args:?} failed");
        };
        let p = |p: &Path| p.to_str().unwrap().to_string();
        run(&["genpkey", "-algorithm", "ed25519", "-out", &p(&key)]);
        run(&[
            "pkeyutl",
            "-sign",
            "-rawin",
            "-inkey",
            &p(&key),
            "-in",
            &p(&sums),
            "-out",
            &p(&sig),
        ]);
        run(&[
            "pkey",
            "-in",
            &p(&key),
            "-pubout",
            "-outform",
            "DER",
            "-out",
            &p(&pubder),
        ]);
        let der = fs::read(&pubder).unwrap();
        let public: [u8; 32] = der[der.len() - 32..].try_into().unwrap();
        let sums = fs::read(&sums).unwrap();
        verify_sums(&sums, &fs::read(&sig).unwrap(), &[public]).unwrap();
        let mut tampered = sums.clone();
        tampered[0] ^= 1;
        assert!(verify_sums(&tampered, &fs::read(&sig).unwrap(), &[public]).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The embedded key must be the one published in the repository (the
    /// release workflow checks its secret against the same file).
    #[test]
    fn embedded_keys_match_published_pem() {
        let pem = include_str!("../docs/release-signing-key.pub.pem");
        let mut keys = Vec::new();
        for block in pem.split("-----END PUBLIC KEY-----") {
            let b64: String = block
                .lines()
                .filter(|l| !l.starts_with("-----") && !l.trim().is_empty())
                .collect();
            if b64.is_empty() {
                continue;
            }
            let der = base64_decode(&b64);
            // SubjectPublicKeyInfo for Ed25519: 12-byte prefix, then the key.
            assert_eq!(der.len(), 44);
            assert_eq!(
                &der[..12],
                &[
                    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00
                ]
            );
            keys.push(<[u8; 32]>::try_from(&der[12..]).unwrap());
        }
        assert_eq!(keys, RELEASE_PUBLIC_KEYS);
        for key in RELEASE_PUBLIC_KEYS {
            assert!(VerifyingKey::from_bytes(key).is_ok());
        }
    }

    fn base64_decode(s: &str) -> Vec<u8> {
        const ABC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = Vec::new();
        let (mut acc, mut bits) = (0u32, 0);
        for c in s.bytes().filter(|&c| c != b'=') {
            acc = acc << 6 | ABC.iter().position(|&a| a == c).unwrap() as u32;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((acc >> bits) as u8);
            }
        }
        out
    }

    const TOP: &str = "ani-gui-1.1.0-x86_64-unknown-linux-gnu";

    fn extract(archive: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        extract_binary(archive, TOP, &mut out, 1 << 20)?;
        Ok(out)
    }

    #[test]
    fn extraction_reads_only_bin_ani_gui() {
        let good = tarball(&[
            (
                &format!("{TOP}/share/applications/x.desktop"),
                b"[Desktop Entry]",
            ),
            (&format!("{TOP}/bin/ani-gui"), ELF),
        ]);
        assert_eq!(extract(&good).unwrap(), ELF);
        // "./" prefixes are harmless.
        let dotted = tarball(&[(&format!("./{TOP}/bin/ani-gui"), ELF)]);
        assert_eq!(extract(&dotted).unwrap(), ELF);
        // Another version's folder, or no binary at all.
        assert!(extract(&tarball(&[("ani-gui-9.9.9-x/bin/ani-gui", ELF)])).is_err());
        assert!(extract(&tarball(&[(&format!("{TOP}/README"), b"hi")])).is_err());
        // Two copies: refuse rather than guess.
        let twice = tarball(&[
            (&format!("{TOP}/bin/ani-gui"), ELF),
            (&format!("{TOP}/bin/ani-gui"), ELF),
        ]);
        assert!(extract(&twice).is_err());
        // Too large.
        let mut out = Vec::new();
        assert!(extract_binary(&good[..], TOP, &mut out, 4).is_err());
        // Not a gzip at all.
        assert!(extract(b"plain").is_err());
    }

    #[test]
    fn extraction_rejects_path_traversal_and_links() {
        let regular = tar::EntryType::Regular;
        for name in [
            format!("{TOP}/../{TOP}/bin/ani-gui"),
            "../../bin/ani-gui".to_string(),
            format!("/{TOP}/bin/ani-gui"),
            format!("{TOP}/bin/../../../.bashrc"),
        ] {
            let archive = raw_tarball(&name, regular, ELF);
            let err = extract(&archive).unwrap_err().to_string();
            assert!(err.contains("unsafe path"), "{name}: {err}");
        }
        let link = raw_tarball(&format!("{TOP}/bin/ani-gui"), tar::EntryType::Symlink, b"");
        assert!(extract(&link).is_err());
    }

    #[test]
    fn dev_builds_and_unwritable_installs_only_notify() {
        let dir = temp_dir("eligible");
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("ani-gui");
        fs::write(&exe, ELF).unwrap();

        assert_eq!(self_update_path(&exe, true), Err(NotEligible::DevBuild));
        assert!(in_cargo_target(Path::new(
            "/src/ani-gui/target/release/ani-gui"
        )));
        assert!(in_cargo_target(Path::new(
            "/src/ani-gui/target/debug/ani-gui"
        )));
        assert!(in_cargo_target(Path::new(
            "/s/target/x86_64-unknown-linux-gnu/release/ani-gui"
        )));
        assert!(!in_cargo_target(Path::new("/home/u/.local/bin/ani-gui")));
        assert!(!in_cargo_target(Path::new("/home/u/target/bin/ani-gui")));

        if euid() == 0 {
            assert_eq!(self_update_path(&exe, false), Err(NotEligible::Root));
        } else {
            assert_eq!(self_update_path(&exe, false), Ok(exe.clone()));
            fs::set_permissions(&bin, Permissions::from_mode(0o555)).unwrap();
            assert_eq!(self_update_path(&exe, false), Err(NotEligible::NotWritable));
            fs::set_permissions(&bin, Permissions::from_mode(0o755)).unwrap();
            // A root-owned system binary, e.g. /usr/bin/env.
            let system = Path::new("/usr/bin/env");
            if system.exists() {
                assert!(self_update_path(system, false).is_err());
            }
        }
        assert_eq!(
            self_update_path(&dir.join("missing"), false).unwrap_err(),
            if euid() == 0 {
                NotEligible::Root
            } else {
                NotEligible::UnknownPath
            }
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".ani-gui-"))
            .collect()
    }

    #[test]
    fn replace_is_atomic_and_failures_leave_the_old_binary() {
        let dir = temp_dir("replace");
        let exe = dir.join("ani-gui");
        fs::write(&exe, b"\x7fELF old").unwrap();
        fs::set_permissions(&exe, Permissions::from_mode(0o700)).unwrap();
        // A process running the old binary keeps reading the old inode.
        let mut running = File::open(&exe).unwrap();

        let archive = tarball(&[(&format!("{TOP}/bin/ani-gui"), ELF)]);
        replace_binary(&archive[..], TOP, &exe).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), ELF);
        assert_eq!(fs::metadata(&exe).unwrap().mode() & 0o777, 0o755);
        let mut old = Vec::new();
        running.read_to_end(&mut old).unwrap();
        assert_eq!(old, b"\x7fELF old");
        assert!(leftovers(&dir).is_empty());

        // Not an executable: refused, old binary intact, no temp files.
        let script = tarball(&[(&format!("{TOP}/bin/ani-gui"), b"#!/bin/sh\nrm -rf ~\n")]);
        assert!(replace_binary(&script[..], TOP, &exe).is_err());
        let empty = tarball(&[(&format!("{TOP}/bin/ani-gui"), b"")]);
        assert!(replace_binary(&empty[..], TOP, &exe).is_err());
        let broken = &archive[..archive.len() / 2];
        assert!(replace_binary(broken, TOP, &exe).is_err());
        assert_eq!(fs::read(&exe).unwrap(), ELF);
        assert!(leftovers(&dir).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn checks_are_due_every_six_hours() {
        assert!(check_due(None, 1_000));
        assert!(!check_due(Some(1_000), 1_000 + CHECK_EVERY_SECS - 1));
        assert!(check_due(Some(1_000), 1_000 + CHECK_EVERY_SECS));
        assert!(check_due(Some(5_000), 1_000));
        assert_eq!(ago(100, 130), "just now");
        assert_eq!(ago(0, 60), "1 minute ago");
        assert_eq!(ago(0, 7_200), "2 hours ago");
        assert_eq!(ago(0, 86_400 * 3), "3 days ago");
        assert_eq!(ago(100, 50), "just now");
    }

    #[test]
    fn saved_state_roundtrips_and_tolerates_garbage() {
        let dir = temp_dir("state");
        let path = dir.join("nested/update-state.json");
        assert_eq!(load_saved(&path), Saved::default());
        let saved = Saved {
            last_check: Some(42),
            result: Some("Up to date".into()),
            installed: Some("1.1.0".into()),
        };
        store_saved(&path, &saved).unwrap();
        assert_eq!(load_saved(&path), saved);
        fs::write(&path, b"{oops").unwrap();
        assert_eq!(load_saved(&path), Saved::default());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Serves `routes` (path → body) over plain HTTP on 127.0.0.1.
    fn serve(listener: TcpListener, routes: Vec<(String, Vec<u8>)>) {
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = io::BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                if reader.read_line(&mut request).is_err() {
                    continue;
                }
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let path = request.split_whitespace().nth(1).unwrap_or("");
                let reply = match routes.iter().find(|(p, _)| p == path) {
                    Some((_, body)) => {
                        let mut r = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        r.extend_from_slice(body);
                        r
                    }
                    None => {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    }
                };
                let _ = stream.write_all(&reply);
            }
        });
    }

    /// A fake GitHub with release v99.0.0 signed by `key`; `archive_bytes`
    /// replaces the served archive after signing when given.
    fn fake_github(key: &SigningKey, served_archive: Option<Vec<u8>>) -> Source {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let prefix = format!("{base}/download/");
        let version = "99.0.0";
        let top = format!("ani-gui-{version}-{T}");
        let name = format!("{top}.tar.gz");
        let archive = tarball(&[
            (
                &format!("{top}/share/applications/io.github.sweettooth05.AniGui.desktop"),
                b"[Desktop Entry]",
            ),
            (&format!("{top}/bin/ani-gui"), ELF),
        ]);
        let sums = format!("{}  {name}\n", sha256(&archive));
        let sig = key.sign(sums.as_bytes()).to_bytes().to_vec();
        let url = |n: &str| format!("{prefix}v{version}/{n}");
        let json = release_json(
            &format!("v{version}"),
            false,
            false,
            &[(&name, &url(&name)), (SUMS, &url(SUMS)), (SIG, &url(SIG))],
        );
        let path = |n: &str| format!("/download/v{version}/{n}");
        serve(
            listener,
            vec![
                ("/releases/latest".into(), json),
                (path(&name), served_archive.unwrap_or(archive)),
                (path(SUMS), sums.into_bytes()),
                (path(SIG), sig),
            ],
        );
        Source {
            api_url: format!("{base}/releases/latest"),
            download_prefix: prefix,
            https_only: false,
        }
    }

    /// The whole flow against a local server: check, download, verify,
    /// extract and swap a copy of a real binary.
    #[test]
    fn end_to_end_update_replaces_a_copied_binary() {
        let key = test_key(42);
        let public = key.verifying_key().to_bytes();
        let source = fake_github(&key, None);
        let dir = temp_dir("e2e");
        let exe = dir.join("ani-gui");
        fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
        let before = fs::read(&exe).unwrap();

        // Nothing newer than 99.0.0 itself.
        assert_eq!(latest_newer(&source, v("99.0.0")).unwrap(), None);
        let release = latest_newer(&source, v("1.0.0")).unwrap().unwrap();
        assert_eq!(release.version, v("99.0.0"));

        // The wrong key refuses the release and keeps the binary.
        let other = test_key(1).verifying_key().to_bytes();
        let err = install(&source, &release, T, &exe, &[other]).unwrap_err();
        assert!(format!("{err:#}").contains("isn't signed"), "{err:#}");
        assert_eq!(fs::read(&exe).unwrap(), before);

        install(&source, &release, T, &exe, &[public]).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), ELF);
        assert_eq!(fs::metadata(&exe).unwrap().mode() & 0o777, 0o755);
        assert!(leftovers(&dir).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn end_to_end_tampered_archive_is_refused() {
        let key = test_key(42);
        let evil = tarball(&[(
            &format!("ani-gui-99.0.0-{T}/bin/ani-gui"),
            b"\x7fELF evil" as &[u8],
        )]);
        let source = fake_github(&key, Some(evil));
        let dir = temp_dir("e2e-evil");
        let exe = dir.join("ani-gui");
        fs::write(&exe, b"\x7fELF old").unwrap();
        let release = latest_newer(&source, v("1.0.0")).unwrap().unwrap();
        let err = install(
            &source,
            &release,
            T,
            &exe,
            &[key.verifying_key().to_bytes()],
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("doesn't match its signed checksum"),
            "{err:#}"
        );
        assert_eq!(fs::read(&exe).unwrap(), b"\x7fELF old");
        assert!(leftovers(&dir).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Feeds a folder the release workflow produced (the archive,
    /// `SHA256SUMS` and `SHA256SUMS.sig`) through the real updater with the
    /// embedded key. Run by hand before a first release:
    /// `ANI_GUI_DIST=dist cargo test release_artifacts -- --ignored`
    #[test]
    #[ignore = "needs ANI_GUI_DIST pointing at signed release artifacts"]
    fn release_artifacts_install_with_the_embedded_key() {
        let dist = PathBuf::from(std::env::var("ANI_GUI_DIST").expect("set ANI_GUI_DIST"));
        let archive = fs::read_dir(&dist)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .find(|n| n.ends_with(&format!("-{TARGET}.tar.gz")))
            .expect("no archive for this target");
        let version = archive
            .strip_prefix("ani-gui-")
            .and_then(|n| n.strip_suffix(&format!("-{TARGET}.tar.gz")))
            .unwrap()
            .to_string();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let prefix = format!("{base}/download/");
        let url = |n: &str| format!("{prefix}v{version}/{n}");
        let json = release_json(
            &format!("v{version}"),
            false,
            false,
            &[
                (&archive, &url(&archive)),
                (SUMS, &url(SUMS)),
                (SIG, &url(SIG)),
            ],
        );
        let routes = [archive.as_str(), SUMS, SIG]
            .iter()
            .map(|n| {
                (
                    format!("/download/v{version}/{n}"),
                    fs::read(dist.join(n)).unwrap(),
                )
            })
            .chain([("/releases/latest".to_string(), json)])
            .collect();
        serve(listener, routes);
        let source = Source {
            api_url: format!("{base}/releases/latest"),
            download_prefix: prefix,
            https_only: false,
        };
        let dir = temp_dir("dist");
        let exe = dir.join("ani-gui");
        fs::write(&exe, b"\x7fELF old").unwrap();
        let release = latest_newer(&source, v("0.0.1")).unwrap().unwrap();
        install(&source, &release, TARGET, &exe, RELEASE_PUBLIC_KEYS).unwrap();
        let installed = fs::read(&exe).unwrap();
        assert_eq!(&installed[..4], b"\x7fELF");
        assert!(installed.len() > 1 << 20);
        fs::remove_dir_all(&dir).unwrap();
    }
}
