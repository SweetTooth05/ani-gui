use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use gpui::{
    AbsoluteLength, Animation, AnimationExt, App, Asset, Div, EdgesRefinement, Hsla,
    ImageAssetLoader, ImageCacheError, Img, IntoElement, ObjectFit, Pixels, RenderImage, Resource,
    SharedString, Window, div, img, prelude::*, pulsating_between,
};

use crate::app::AppState;
use crate::ui::theme;
use crate::{images, paths};

/// URL → local file, for every image fetched this session.
static LOCAL: LazyLock<Mutex<HashMap<String, PathBuf>>> = LazyLock::new(Default::default);

/// Downloads (once, into the image cache) and decodes a remote image. gpui
/// runs `load`'s future on its background executor and caches the result.
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
        // `images::cached_image` stores `url` at `dir/cache_key(url)`.
        let path: Arc<Path> = dir.join(images::cache_key(&url)).into();
        let decode = <ImageAssetLoader as Asset>::load(Resource::Path(path), cx);
        async move {
            fetch(&url, &dir).map_err(|err| ImageCacheError::Other(Arc::new(err)))?;
            decode.await
        }
    }
}

fn fetch(url: &str, dir: &Path) -> anyhow::Result<()> {
    let local = || LOCAL.lock().unwrap_or_else(PoisonError::into_inner);
    if local().contains_key(url) {
        return Ok(());
    }
    let path = images::cached_image(url, dir)?;
    local().insert(url.to_string(), path);
    Ok(())
}

/// An image filling its parent (cover fit), with a shimmer while it loads
/// and a plain tile when there is none.
pub fn remote_image(url: &str) -> Img {
    let url = SharedString::from(url.to_string());
    img(move |window: &mut Window, cx: &mut App| {
        if url.is_empty() {
            return Some(Err(ImageCacheError::Asset("no image".into())));
        }
        window.use_asset::<RemoteImage>(&url, cx)
    })
    .size_full()
    .object_fit(ObjectFit::Cover)
    .with_loading(|| shimmer().into_any_element())
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
