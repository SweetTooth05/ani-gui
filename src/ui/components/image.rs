use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AbsoluteLength, Animation, AnimationExt, App, Asset, Div, EdgesRefinement, Global, Hsla,
    ImageCacheError, Img, IntoElement, ObjectFit, Pixels, RenderImage, SharedString, Window, div,
    img, prelude::*, pulsating_between,
};

use crate::app::AppState;
use crate::ui::theme;
use crate::{images, paths};

/// Decoded images kept in memory; older ones are decoded again from disk.
const KEEP_DECODED: usize = 256;
const RETRY_FIRST: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(60);

/// Downloads (once, into the image cache) and decodes a remote image on our
/// dedicated image worker pools so GPUI's background executor stays free for
/// database and API tasks.
struct RemoteImage;

impl Asset for RemoteImage {
    type Source = SharedString;
    type Output = Result<Arc<RenderImage>, ImageCacheError>;

    fn load(
        url: Self::Source,
        cx: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        let dir = cx
            .try_global::<AppState>()
            .map_or_else(paths::cache_dir, |s| s.cache_dir.clone())
            .join("images");
        let load = images::load_render_image(url.to_string(), dir);
        async move { load.await.map_err(|err| ImageCacheError::Asset(err.into())) }
    }
}

/// Which decoded images to keep (the least recently drawn go first once
/// there are more than `capacity`) and when failed URLs may be tried again.
struct ImagePolicy<T> {
    capacity: usize,
    tick: u64,
    loaded: HashMap<String, (u64, T)>,
    /// URL → (earliest retry, failures in a row).
    failed: HashMap<String, (Instant, u32)>,
}

impl<T: Clone> ImagePolicy<T> {
    fn new(capacity: usize) -> Self {
        ImagePolicy {
            capacity,
            tick: 0,
            loaded: HashMap::new(),
            failed: HashMap::new(),
        }
    }

    /// O(1) lookup for an already-decoded image, bumping its recency tick.
    fn get_loaded(&mut self, url: &str) -> Option<T> {
        let entry = self.loaded.get_mut(url)?;
        self.tick += 1;
        entry.0 = self.tick;
        Some(entry.1.clone())
    }

    /// Notes that `url` was drawn; returns the image evicted to make room.
    fn drawn(&mut self, url: &str, image: &T) -> Option<(String, T)> {
        self.tick += 1;
        if let Some(entry) = self.loaded.get_mut(url) {
            entry.0 = self.tick;
            return None;
        }
        self.failed.remove(url);
        self.loaded
            .insert(url.to_string(), (self.tick, image.clone()));
        if self.loaded.len() <= self.capacity {
            return None;
        }
        let oldest = self
            .loaded
            .iter()
            .min_by_key(|(_, (tick, _))| *tick)
            .map(|(url, _)| url.clone())?;
        self.loaded
            .remove(&oldest)
            .map(|(_, image)| (oldest, image))
    }

    /// Notes a failed load; the wait before the next try doubles each time.
    fn failed(&mut self, url: &str, now: Instant) {
        let failures = self.failed.get(url).map_or(0, |(_, n)| *n) + 1;
        let wait = RETRY_FIRST
            .saturating_mul(1 << (failures - 1).min(16))
            .min(RETRY_MAX);
        self.failed.insert(url.to_string(), (now + wait, failures));
    }

    fn waiting(&self, url: &str, now: Instant) -> bool {
        self.failed.get(url).is_some_and(|(retry, _)| now < *retry)
    }
}

struct Images(ImagePolicy<Arc<RenderImage>>);

impl Default for Images {
    fn default() -> Self {
        Images(ImagePolicy::new(KEEP_DECODED))
    }
}

impl Global for Images {}

/// Loads `url` for this frame: serves already-decoded images in O(1) without
/// touching GPUI's asset table, evicts the least recently drawn image once too
/// many are decoded, and forgets failures so a later frame retries.
fn use_remote(
    url: &SharedString,
    window: &mut Window,
    cx: &mut App,
) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
    if let Some(image) = cx.default_global::<Images>().0.get_loaded(url) {
        return Some(Ok(image));
    }
    let now = Instant::now();
    if cx.default_global::<Images>().0.waiting(url, now) {
        return Some(Err(ImageCacheError::Asset("waiting to retry".into())));
    }
    let result = window.use_asset::<RemoteImage>(url, cx)?;
    // Once resolved, Images owns the cache entry; removing from GPUI's asset
    // table avoids duplicate references and per-frame Box/Any round-trips.
    cx.remove_asset::<RemoteImage>(url);
    match &result {
        Ok(image) => {
            if let Some((_, old)) = cx.default_global::<Images>().0.drawn(url, image) {
                cx.drop_image(old, Some(window));
            }
        }
        Err(_) => {
            cx.default_global::<Images>().0.failed(url, now);
        }
    }
    Some(result)
}

/// An image filling its parent (cover fit), with a quiet surface tile while it
/// loads or when unavailable.
pub fn remote_image(url: &str) -> Img {
    let url = SharedString::from(url.to_string());
    img(move |window: &mut Window, cx: &mut App| {
        if url.is_empty() {
            return Some(Err(ImageCacheError::Asset("no image".into())));
        }
        use_remote(&url, window, cx)
    })
    .size_full()
    .object_fit(ObjectFit::Cover)
    .with_loading(|| div().size_full().bg(theme::raised()).into_any_element())
    .with_fallback(|| div().size_full().bg(theme::raised()).into_any_element())
}

/// `remote_image` cropped to its parent with rounded corners. gpui clips
/// only to rectangles, so the corners are painted over in `backdrop` (the
/// color behind the image; `hover` swaps it while `group` is hovered) by a
/// border whose inner edge is the rounded rectangle.
pub fn rounded_image(
    url: &str,
    radius: Pixels,
    backdrop: impl Into<Hsla>,
    hover: Option<(&'static str, Hsla)>,
) -> Div {
    let mut mask = div()
        .absolute()
        .top(-radius)
        .left(-radius)
        .right(-radius)
        .bottom(-radius)
        .rounded(radius * 2.)
        .border_color(backdrop.into());
    let width = Some(AbsoluteLength::Pixels(radius));
    mask.style().border_widths = EdgesRefinement {
        top: width,
        right: width,
        bottom: width,
        left: width,
    };
    if let Some((group, color)) = hover {
        mask = mask.group_hover(group, move |s| s.border_color(color));
    }
    div()
        .relative()
        .size_full()
        .overflow_hidden()
        .child(remote_image(url))
        .child(mask)
}

pub fn shimmer() -> impl IntoElement {
    div().size_full().bg(theme::raised()).with_animation(
        "shimmer",
        Animation::new(Duration::from_millis(1400))
            .repeat()
            .with_easing(pulsating_between(0.35, 0.9)),
        |this, delta| this.opacity(delta),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_the_least_recently_drawn() {
        let mut policy = ImagePolicy::new(2);
        assert_eq!(policy.drawn("a", &1), None);
        assert_eq!(policy.drawn("b", &2), None);
        assert_eq!(policy.drawn("a", &1), None);
        assert_eq!(policy.drawn("c", &3), Some(("b".to_string(), 2)));
        assert_eq!(policy.drawn("a", &1), None);
        assert_eq!(policy.drawn("b", &2), Some(("c".to_string(), 3)));
    }

    #[test]
    fn failed_urls_wait_longer_after_each_failure() {
        let mut policy = ImagePolicy::<u8>::new(2);
        let t0 = Instant::now();
        assert!(!policy.waiting("a", t0));
        policy.failed("a", t0);
        assert!(policy.waiting("a", t0 + Duration::from_secs(1)));
        assert!(!policy.waiting("a", t0 + RETRY_FIRST));
        policy.failed("a", t0);
        assert!(policy.waiting("a", t0 + RETRY_FIRST));
        assert!(!policy.waiting("a", t0 + RETRY_FIRST * 2));
        for _ in 0..20 {
            policy.failed("a", t0);
        }
        assert!(!policy.waiting("a", t0 + RETRY_MAX));
        assert!(!policy.waiting("b", t0));
    }

    #[test]
    fn a_successful_load_clears_the_failure() {
        let mut policy = ImagePolicy::new(2);
        let t0 = Instant::now();
        policy.failed("a", t0);
        policy.drawn("a", &1);
        assert!(!policy.waiting("a", t0));
    }
}
