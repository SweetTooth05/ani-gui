use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};

use crate::{allanime, net};

const ALLANIME_HEADERS: &[(&str, &str)] = &[
    ("User-Agent", allanime::AGENT),
    ("Referer", allanime::REFERER),
];

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

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
    fs::create_dir_all(cache_dir).with_context(|| format!("creating {}", cache_dir.display()))?;
    let tmp = cache_dir.join(format!(
        ".{key}.{}.{}.tmp",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;
    if let Err(err) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("saving {}", path.display()));
    }
    Ok(path)
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
}
