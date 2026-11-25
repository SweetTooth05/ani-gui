use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, mpsc};
use std::thread;

use anyhow::{Context, Result, bail};
use futures::channel::oneshot;
use gpui::RenderImage;

use crate::{allanime, net};

const ALLANIME_HEADERS: &[(&str, &str)] = &[
    ("User-Agent", allanime::AGENT),
    ("Referer", allanime::REFERER),
];

const DISK_WORKERS: usize = 3;
const NET_WORKERS: usize = 6;

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

type ImageResult = std::result::Result<Arc<RenderImage>, String>;
type Waiters = Vec<oneshot::Sender<ImageResult>>;

struct Job {
    url: String,
    cache_dir: PathBuf,
    key: String,
}

static IN_FLIGHT: LazyLock<Mutex<HashMap<String, Waiters>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static DISK_POOL: LazyLock<mpsc::Sender<Job>> = LazyLock::new(|| {
    spawn_pool("ani-img-disk", DISK_WORKERS, |job| {
        let path = job.cache_dir.join(&job.key);
        match fs::read(&path) {
            Ok(bytes) if !bytes.is_empty() => match decode_render_image(&bytes) {
                Ok(img) => {
                    finish_in_flight(&job.url, Ok(img));
                    return;
                }
                Err(_) => {
                    // Corrupt or truncated cache entry: remove and re-fetch once.
                    let _ = fs::remove_file(&path);
                }
            },
            _ => {}
        }
        if NET_POOL.send(job).is_err() {
            // Unreachable unless process is tearing down.
        }
    })
});

static NET_POOL: LazyLock<mpsc::Sender<Job>> = LazyLock::new(|| {
    spawn_pool("ani-img-net", NET_WORKERS, |job| {
        let result = fetch_and_decode(&job);
        finish_in_flight(&job.url, result.map_err(|err| format!("{err:#}")));
    })
});

fn spawn_pool(name: &'static str, workers: usize, handle: fn(Job)) -> mpsc::Sender<Job> {
    let (tx, rx) = mpsc::channel::<Job>();
    let rx = Arc::new(Mutex::new(rx));
    for i in 0..workers {
        let rx = rx.clone();
        let _ = thread::Builder::new()
            .name(format!("{name}-{i}"))
            .spawn(move || {
                loop {
                    let job = {
                        let guard = rx.lock().unwrap_or_else(|e| e.into_inner());
                        guard.recv()
                    };
                    match job {
                        Ok(job) => handle(job),
                        Err(_) => break,
                    }
                }
            });
    }
    tx
}

fn finish_in_flight(url: &str, result: ImageResult) {
    let waiters = IN_FLIGHT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(url);
    if let Some(waiters) = waiters {
        for tx in waiters {
            let _ = tx.send(result.clone());
        }
    }
}

fn fetch_and_decode(job: &Job) -> Result<Arc<RenderImage>> {
    let bytes = net::get_bytes(&job.url, request_headers(&job.url))?;
    if bytes.is_empty() {
        bail!("empty image from {}", job.url);
    }
    let img = decode_render_image(&bytes).with_context(|| format!("decoding {}", job.url))?;
    if let Err(err) = save_atomic(&job.cache_dir, &job.key, &bytes) {
        eprintln!("ani-gui: caching image {}: {err:#}", job.url);
    }
    Ok(img)
}

fn decode_render_image(bytes: &[u8]) -> Result<Arc<RenderImage>> {
    let bgra = decode_bgra(bytes)?;
    Ok(Arc::new(RenderImage::new(vec![image::Frame::new(bgra)])))
}

fn decode_bgra(bytes: &[u8]) -> Result<image::RgbaImage> {
    let mut rgba = image::load_from_memory(bytes)
        .context("decoding image")?
        .into_rgba8();
    // gpui expects BGRA8 pixel layout.
    for px in rgba.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    Ok(rgba)
}

fn save_atomic(cache_dir: &Path, key: &str, bytes: &[u8]) -> Result<PathBuf> {
    let path = cache_dir.join(key);
    fs::create_dir_all(cache_dir).with_context(|| format!("creating {}", cache_dir.display()))?;
    let tmp = cache_dir.join(format!(
        ".{key}.{}.{}.tmp",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    if let Err(err) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("saving {}", path.display()));
    }
    Ok(path)
}

/// Loads and decodes `url` on dedicated image worker threads (separate from
/// GPUI's background executor), deduplicating concurrent requests for the same
/// URL and serving disk cache hits without waiting behind network fetches.
pub fn load_render_image(
    url: String,
    cache_dir: PathBuf,
) -> impl Future<Output = ImageResult> + Send + 'static {
    let (tx, rx) = oneshot::channel();
    let should_enqueue = {
        let mut map = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        match map.get_mut(&url) {
            Some(waiters) => {
                waiters.push(tx);
                false
            }
            None => {
                map.insert(url.clone(), vec![tx]);
                true
            }
        }
    };
    if should_enqueue {
        let key = cache_key(&url);
        let url_for_err = url.clone();
        if DISK_POOL
            .send(Job {
                url,
                cache_dir,
                key,
            })
            .is_err()
        {
            finish_in_flight(&url_for_err, Err("image worker pool stopped".into()));
        }
    }
    async move {
        rx.await
            .unwrap_or_else(|_| Err("image load cancelled".into()))
    }
}

/// FNV-1a 64 of the URL in hex, plus the URL's file extension (lowercased).
pub fn cache_key(url: &str) -> String {
    let hash = url.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{hash:016x}{}", extension(url))
}

fn extension(url: &str) -> String {
    let path = match url.split_once("://") {
        Some((_, rest)) => rest.find('/').map_or("", |i| &rest[i..]),
        None => url,
    };
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let name = path.rsplit('/').next().unwrap_or_default();
    match name.rsplit_once('.') {
        Some((stem, ext))
            if !stem.is_empty()
                && (1..=5).contains(&ext.len())
                && ext.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            format!(".{}", ext.to_ascii_lowercase())
        }
        _ => String::new(),
    }
}

/// Path of the local copy of `url` in `cache_dir`, downloading it on first use.
#[allow(dead_code)]
pub fn cached_image(url: &str, cache_dir: &Path) -> Result<PathBuf> {
    let key = cache_key(url);
    let path = cache_dir.join(&key);
    if path.is_file() {
        return Ok(path);
    }
    let bytes = net::get_bytes(url, request_headers(url))?;
    if bytes.is_empty() {
        bail!("empty image from {url}");
    }
    save_atomic(cache_dir, &key, &bytes)
}

/// AllAnime's cover CDN rejects requests without its Referer.
fn request_headers(url: &str) -> &'static [(&'static str, &'static str)] {
    if host(url).is_some_and(|h| Some(h) == host(allanime::COVER_CDN)) {
        ALLANIME_HEADERS
    } else {
        &[]
    }
}

fn host(url: &str) -> Option<&str> {
    url.split_once("://")?.1.split(['/', '?', '#']).next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_stable_and_keeps_ext() {
        assert_eq!(cache_key("a"), "af63dc4c8601ec8c");
        let url = "https://s4.anilist.co/file/anilistcdn/media/anime/cover/large/bx154587-qQTzQnEJJ3oB.jpg";
        let key = cache_key(url);
        assert_eq!(key, cache_key(url));
        assert_eq!(key.len(), 16 + ".jpg".len());
        assert!(key.ends_with(".jpg"));
        assert_ne!(key, cache_key(&url.replace("bx154587", "bx154588")));
        assert!(cache_key("https://x.com/a/b.PNG?w=300#top").ends_with(".png"));
        assert_eq!(cache_key("https://x.com/a/cover").len(), 16);
        assert_eq!(cache_key("https://x.com/a.b/cover").len(), 16);
        assert_eq!(cache_key("https://x.com/a/c.not-an-ext").len(), 16);
    }

    #[test]
    fn cover_cdn_gets_allanime_referer() {
        let cdn = request_headers(
            "https://wp.youtube-anime.com/aln.youtube-anime.com/mcovers/a_tbs/dhw/x.png",
        );
        assert!(cdn.contains(&("Referer", "https://youtu-chan.com")));
        assert!(request_headers("https://s4.anilist.co/file/x.jpg").is_empty());
        assert!(request_headers("https://evil.example/wp.youtube-anime.com/x.png").is_empty());
    }

    #[test]
    fn cached_image_reuses_existing_file() {
        let dir = std::env::temp_dir().join(format!("ani-gui-img-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let url = "http://127.0.0.1:9/never-fetched.jpg";
        std::fs::write(dir.join(cache_key(url)), b"jpeg").unwrap();
        assert_eq!(cached_image(url, &dir).unwrap(), dir.join(cache_key(url)));
        assert!(cached_image("http://127.0.0.1:9/missing.jpg", &dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn decode_bgra_swaps_red_and_blue_channels() {
        let mut src = image::RgbaImage::new(1, 1);
        src.put_pixel(0, 0, image::Rgba([0x11, 0x22, 0x33, 0xff]));
        let mut png = std::io::Cursor::new(Vec::new());
        src.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let bgra = decode_bgra(&png.into_inner()).unwrap();
        assert_eq!(bgra.get_pixel(0, 0).0, [0x33, 0x22, 0x11, 0xff]);
    }
}
