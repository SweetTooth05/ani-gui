//! Design tokens. Dark only: poster and banner art supply the color, the UI
//! stays a quiet night-blue, and a single lantern-amber accent marks what
//! plays (Play buttons, progress, the current page).

use std::borrow::Cow;

use gpui::{App, AssetSource, Hsla, Pixels, Rgba, SharedString, px, rgb, rgba};

/// The page behind everything.
pub fn night() -> Rgba {
    rgb(0x0e1018)
}
/// Sidebar, a shade deeper than the page.
pub fn deep() -> Rgba {
    rgb(0x0a0c12)
}
/// Controls, chips, placeholders.
pub fn raised() -> Rgba {
    rgb(0x1c2030)
}
pub fn raised_hover() -> Rgba {
    rgb(0x272c3f)
}
/// Translucent `raised`, for hovering quiet items.
pub fn wash() -> Rgba {
    rgba(0x1c203080)
}
/// Hairlines, used sparingly.
pub fn line() -> Rgba {
    rgb(0x272c3d)
}
pub fn text() -> Rgba {
    rgb(0xeef0f6)
}
pub fn text_dim() -> Rgba {
    rgb(0xa3a9bc)
}
pub fn text_muted() -> Rgba {
    rgb(0x6b7186)
}
pub fn accent() -> Rgba {
    rgb(0xffc24b)
}
pub fn accent_hover() -> Rgba {
    rgb(0xffd27a)
}
/// Text on the accent.
pub fn on_accent() -> Rgba {
    rgb(0x1d1503)
}
pub fn danger() -> Rgba {
    rgb(0xff8a80)
}
/// `night` at `alpha`, for gradients over art.
pub fn night_alpha(alpha: f32) -> Hsla {
    let mut c: Hsla = night().into();
    c.a = alpha;
    c
}
pub fn scrim() -> Rgba {
    rgba(0x05060acc)
}

pub const UI_FONT: &str = "Barlow";
pub const DISPLAY_FONT: &str = "Barlow Condensed";

/// Type scale (px). Display sizes use `DISPLAY_FONT`.
pub const HERO_TITLE: f32 = 64.;
pub const PAGE_TITLE: f32 = 48.;
pub const SECTION: f32 = 26.;
pub const BODY: f32 = 15.;
pub const SMALL: f32 = 13.;

/// Left edge of page content, shared by hero text and rows.
pub fn gutter() -> Pixels {
    px(48.)
}
pub const SIDEBAR_WIDTH: f32 = 208.;
pub const POSTER_W: f32 = 160.;
pub const POSTER_H: f32 = 240.;
pub const CARD_W: f32 = 304.;
pub const CARD_H: f32 = 171.;

const FONTS: [&[u8]; 6] = [
    include_bytes!("../../assets/fonts/Barlow-Regular.ttf"),
    include_bytes!("../../assets/fonts/Barlow-Medium.ttf"),
    include_bytes!("../../assets/fonts/Barlow-SemiBold.ttf"),
    include_bytes!("../../assets/fonts/Barlow-Bold.ttf"),
    include_bytes!("../../assets/fonts/BarlowCondensed-SemiBold.ttf"),
    include_bytes!("../../assets/fonts/BarlowCondensed-Bold.ttf"),
];

/// Registers the bundled Barlow faces (OFL, `assets/fonts/OFL.txt`).
pub fn register_fonts(cx: &App) {
    let fonts = FONTS.iter().map(|f| Cow::Borrowed(*f)).collect();
    if let Err(err) = cx.text_system().add_fonts(fonts) {
        eprintln!("ani-gui: loading fonts: {err:#}");
    }
}

/// Asset path of the sidebar logo (compact lockup for dark backgrounds).
pub const LOGO: &str = "brand/ani-gui-compact-reversed.svg";
const LOGO_SVG: &[u8] = include_bytes!("../../assets/brand/svg/ani-gui-compact-reversed.svg");

/// Images embedded in the binary, so an installed `ani-gui` needs no asset
/// directory. `img(LOGO)` goes through GPUI's resource loader, which
/// rasterises SVG in full colour.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        Ok((path == LOGO).then_some(Cow::Borrowed(LOGO_SVG)))
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(if LOGO.starts_with(path) {
            vec![LOGO.into()]
        } else {
            Vec::new()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assets_serve_embedded_logo() {
        let logo = Assets.load(LOGO).expect("load").expect("logo present");
        assert!(logo.starts_with(b"<?xml"));
        assert!(std::str::from_utf8(&logo).expect("utf8").contains("<svg"));
    }

    #[test]
    fn assets_list_logo_and_miss_unknown_paths() {
        assert!(Assets.load("nope.svg").expect("load").is_none());
        assert!(Assets.list("").expect("list").contains(&LOGO.into()));
    }
}
