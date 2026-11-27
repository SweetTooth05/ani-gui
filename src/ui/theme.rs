//! Design tokens. Poster and banner art supply the color; the UI stays a
//! quiet backdrop in the chosen theme, and a single accent marks what plays
//! (Play buttons, progress, the current page).
//!
//! The theme and accent are process-wide (`apply`), so every `theme::x()`
//! call follows the user's choice without threading it through. The video
//! player always draws in the dark Night palette (`set_stage`).

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use gpui::{App, AssetSource, BoxShadow, Hsla, Pixels, Rgba, SharedString, point, px, rgb, rgba};

use crate::model::{Accent, Settings, Theme};

struct Palette {
    night: u32,
    deep: u32,
    raised: u32,
    raised_hover: u32,
    /// RGBA.
    wash: u32,
    line: u32,
    text: u32,
    text_dim: u32,
    text_muted: u32,
    danger: u32,
    /// RGBA.
    scrim: u32,
    light: bool,
}

const NIGHT: Palette = Palette {
    night: 0x0e1018,
    deep: 0x0a0c12,
    raised: 0x1c2030,
    raised_hover: 0x272c3f,
    wash: 0x1c203080,
    line: 0x272c3d,
    text: 0xeef0f6,
    text_dim: 0xa3a9bc,
    text_muted: 0x6b7186,
    danger: 0xff8a80,
    scrim: 0x05060acc,
    light: false,
};

/// Near-black, for OLED screens and dark rooms.
const MIDNIGHT: Palette = Palette {
    night: 0x050507,
    deep: 0x000000,
    raised: 0x15161c,
    raised_hover: 0x202229,
    wash: 0x15161c80,
    line: 0x1d1f26,
    text: 0xf2f3f7,
    text_dim: 0xa0a4b2,
    text_muted: 0x646978,
    danger: 0xff8a80,
    scrim: 0x000000cc,
    light: false,
};

/// Night with a violet cast.
const DUSK: Palette = Palette {
    night: 0x15111f,
    deep: 0x0f0c17,
    raised: 0x251e36,
    raised_hover: 0x312846,
    wash: 0x251e3680,
    line: 0x2e2640,
    text: 0xf1eef8,
    text_dim: 0xb1a8c6,
    text_muted: 0x776d8f,
    danger: 0xff8a96,
    scrim: 0x08060dcc,
    light: false,
};

/// Crisp light UI: cool alabaster canvas, pure white sidebar & cards, deep ink text.
const DAYLIGHT: Palette = Palette {
    night: 0xf6f7f9,
    deep: 0xffffff,
    raised: 0xebedf2,
    raised_hover: 0xe0e3eb,
    wash: 0x0f172a0f,
    line: 0xe2e5ec,
    text: 0x11131a,
    text_dim: 0x464b5c,
    text_muted: 0x63697a,
    danger: 0xc82333,
    scrim: 0x0b0d1466,
    light: true,
};

/// Light-only elevated control & track tones.
const DAYLIGHT_CONTROL: u32 = 0xffffff;
const DAYLIGHT_CONTROL_HOVER: u32 = 0xf2f4f8;
const DAYLIGHT_CONTROL_BORDER: u32 = 0xd8dce6;
const DAYLIGHT_TRACK: u32 = 0xe6e9f0;
const DAYLIGHT_GHOST_HOVER: u32 = 0xe9ecf2;
/// Dark ink backdrop and light text for overlays drawn directly over art in Daylight.
const DAYLIGHT_ART_SCRIM: u32 = 0x0c0e16;
const DAYLIGHT_ON_ART: u32 = 0xf6f7fb;
const DAYLIGHT_ON_ART_DIM: u32 = 0xd0d5e2;
const DAYLIGHT_SHADOW_INK: u32 = 0x0f172a;

/// (accent, hover, text on it), for dark and light palettes.
struct AccentColors {
    dark: (u32, u32, u32),
    light: (u32, u32, u32),
}

fn accent_colors(accent: Accent) -> AccentColors {
    match accent {
        Accent::Amber => AccentColors {
            dark: (0xffc24b, 0xffd27a, 0x1d1503),
            light: (0xb36200, 0x9c5500, 0xffffff),
        },
        Accent::Coral => AccentColors {
            dark: (0xff5c7a, 0xff7d94, 0x2a0710),
            light: (0xd62d4f, 0xbf2242, 0xffffff),
        },
        Accent::Violet => AccentColors {
            dark: (0x8b6cff, 0xa28aff, 0x0f0826),
            light: (0x5e3fd9, 0x4f32c4, 0xffffff),
        },
        Accent::Cyan => AccentColors {
            dark: (0x35d0f0, 0x66dcf4, 0x04222a),
            light: (0x087e99, 0x066c84, 0xffffff),
        },
        Accent::Mint => AccentColors {
            dark: (0x4fe0a6, 0x7ae8bb, 0x052a1b),
            light: (0x0f8558, 0x0c724b, 0xffffff),
        },
    }
}

static THEME: AtomicU8 = AtomicU8::new(0);
static ACCENT: AtomicU8 = AtomicU8::new(0);
/// The video player is showing: draw in Night whatever the theme.
static STAGE: AtomicBool = AtomicBool::new(false);

fn index_of<T: PartialEq>(all: &[T], value: &T) -> u8 {
    all.iter().position(|v| v == value).unwrap_or(0) as u8
}

/// Makes `settings`' theme and accent current. Windows pick it up on their
/// next frame (`App::refresh_windows`).
pub fn apply(settings: &Settings) {
    THEME.store(index_of(Theme::ALL, &settings.theme), Ordering::Relaxed);
    ACCENT.store(index_of(Accent::ALL, &settings.accent), Ordering::Relaxed);
}

/// Called by the root view each frame: whether the player owns the window.
pub fn set_stage(stage: bool) {
    STAGE.store(stage, Ordering::Relaxed);
}

fn current_theme() -> Theme {
    if STAGE.load(Ordering::Relaxed) {
        return Theme::Night;
    }
    Theme::ALL
        .get(THEME.load(Ordering::Relaxed) as usize)
        .copied()
        .unwrap_or(Theme::Night)
}

fn current_accent() -> Accent {
    Accent::ALL
        .get(ACCENT.load(Ordering::Relaxed) as usize)
        .copied()
        .unwrap_or(Accent::Amber)
}

fn palette() -> &'static Palette {
    match current_theme() {
        Theme::Night => &NIGHT,
        Theme::Midnight => &MIDNIGHT,
        Theme::Dusk => &DUSK,
        Theme::Daylight => &DAYLIGHT,
    }
}

fn accent_set() -> (u32, u32, u32) {
    let colors = accent_colors(current_accent());
    if palette().light {
        colors.light
    } else {
        colors.dark
    }
}

/// Whether the current palette is light (dark text on a pale page).
pub fn is_light() -> bool {
    palette().light
}

/// The page behind everything.
pub fn night() -> Rgba {
    rgb(palette().night)
}
/// Sidebar, titlebar and elevated panels (white in Daylight, darker than page in dark themes).
pub fn deep() -> Rgba {
    rgb(palette().deep)
}
/// Subtle neutral surfaces: placeholders, skeletons, hover fills.
pub fn raised() -> Rgba {
    rgb(palette().raised)
}
pub fn raised_hover() -> Rgba {
    rgb(palette().raised_hover)
}
/// Translucent tint for hovering quiet items.
pub fn wash() -> Rgba {
    rgba(palette().wash)
}
/// Hairlines, used sparingly.
pub fn line() -> Rgba {
    rgb(palette().line)
}
/// Elevated control surface: pure white in Daylight, `raised()` in dark themes.
pub fn control() -> Rgba {
    if is_light() {
        rgb(DAYLIGHT_CONTROL)
    } else {
        raised()
    }
}
/// Hover surface for elevated controls.
pub fn control_hover() -> Rgba {
    if is_light() {
        rgb(DAYLIGHT_CONTROL_HOVER)
    } else {
        raised_hover()
    }
}
/// Border for elevated controls in Daylight (`line()` in dark themes).
pub fn control_border() -> Rgba {
    if is_light() {
        rgb(DAYLIGHT_CONTROL_BORDER)
    } else {
        line()
    }
}
/// Recessed neutral track for segmented controls, off switches, and UI progress bars.
pub fn track() -> Rgba {
    if is_light() {
        rgb(DAYLIGHT_TRACK)
    } else {
        raised()
    }
}
/// Hover fill for ghost buttons and quiet icon buttons.
pub fn ghost_hover() -> Rgba {
    if is_light() {
        rgb(DAYLIGHT_GHOST_HOVER)
    } else {
        raised()
    }
}
pub fn text() -> Rgba {
    rgb(palette().text)
}
pub fn text_dim() -> Rgba {
    rgb(palette().text_dim)
}
pub fn text_muted() -> Rgba {
    rgb(palette().text_muted)
}
pub fn accent() -> Rgba {
    rgb(accent_set().0)
}
pub fn accent_hover() -> Rgba {
    rgb(accent_set().1)
}
/// Text on the accent.
pub fn on_accent() -> Rgba {
    rgb(accent_set().2)
}
/// `accent` at `alpha`, for tinted chips and active pills.
pub fn accent_alpha(alpha: f32) -> Hsla {
    let mut c: Hsla = accent().into();
    c.a = alpha;
    c
}
pub fn danger() -> Rgba {
    rgb(palette().danger)
}
/// `night` at `alpha`, for page-edge fades.
pub fn night_alpha(alpha: f32) -> Hsla {
    let mut c: Hsla = night().into();
    c.a = alpha;
    c
}
/// Dark scrim at `alpha` for text/badges drawn over poster or banner art in any theme.
pub fn art_scrim(alpha: f32) -> Hsla {
    let base = if is_light() {
        rgb(DAYLIGHT_ART_SCRIM)
    } else {
        night()
    };
    let mut c: Hsla = base.into();
    c.a = alpha;
    c
}
/// Primary text over dark art scrims (always light).
pub fn on_art() -> Rgba {
    if is_light() {
        rgb(DAYLIGHT_ON_ART)
    } else {
        text()
    }
}
/// Secondary text over dark art scrims (always light).
pub fn on_art_dim() -> Rgba {
    if is_light() {
        rgb(DAYLIGHT_ON_ART_DIM)
    } else {
        text_dim()
    }
}
/// Luminous accent legible over dark art scrims (always uses the dark-palette variant).
pub fn on_art_accent() -> Rgba {
    if is_light() {
        rgb(accent_colors(current_accent()).dark.0)
    } else {
        accent()
    }
}
pub fn scrim() -> Rgba {
    rgba(palette().scrim)
}

fn shadow_ink(alpha: f32) -> Hsla {
    let mut c: Hsla = rgb(DAYLIGHT_SHADOW_INK).into();
    c.a = alpha;
    c
}

/// Subtle elevation shadow for light controls, pills and posters; none in dark themes.
pub fn shadow_sm() -> Vec<BoxShadow> {
    if !is_light() {
        return Vec::new();
    }
    vec![
        BoxShadow {
            color: shadow_ink(0.06),
            offset: point(px(0.), px(1.)),
            blur_radius: px(2.),
            spread_radius: px(0.),
        },
        BoxShadow {
            color: shadow_ink(0.04),
            offset: point(px(0.), px(2.)),
            blur_radius: px(6.),
            spread_radius: px(0.),
        },
    ]
}

/// Soft elevation shadow for light cards and the Home hero; none in dark themes.
pub fn shadow_md() -> Vec<BoxShadow> {
    if !is_light() {
        return Vec::new();
    }
    vec![
        BoxShadow {
            color: shadow_ink(0.05),
            offset: point(px(0.), px(2.)),
            blur_radius: px(4.),
            spread_radius: px(0.),
        },
        BoxShadow {
            color: shadow_ink(0.08),
            offset: point(px(0.), px(8.)),
            blur_radius: px(22.),
            spread_radius: px(-2.),
        },
    ]
}

/// Floating shadow (with a crisp 1 px perimeter ring) for light toasts and dialogs.
pub fn shadow_lg() -> Vec<BoxShadow> {
    if !is_light() {
        return Vec::new();
    }
    vec![
        BoxShadow {
            color: line().into(),
            offset: point(px(0.), px(0.)),
            blur_radius: px(0.),
            spread_radius: px(1.),
        },
        BoxShadow {
            color: shadow_ink(0.10),
            offset: point(px(0.), px(10.)),
            blur_radius: px(28.),
            spread_radius: px(-4.),
        },
        BoxShadow {
            color: shadow_ink(0.06),
            offset: point(px(0.), px(3.)),
            blur_radius: px(8.),
            spread_radius: px(-1.),
        },
    ]
}

/// Swatch of an accent for the Settings picker, as it looks in this theme.
pub fn accent_swatch(accent: Accent) -> Rgba {
    let colors = accent_colors(accent);
    rgb(if is_light() {
        colors.light.0
    } else {
        colors.dark.0
    })
}

/// (page, raised, accent) of `theme`, for the Settings picker.
#[allow(dead_code)]
pub fn theme_swatch(theme: Theme) -> (Rgba, Rgba, Rgba) {
    let p = palette_of(theme);
    let accent = swatch_accent(p.light);
    (rgb(p.night), rgb(p.raised), accent)
}

/// Full miniature palette of `theme` for faithful Settings previews.
pub struct ThemePreview {
    pub page: Rgba,
    pub sidebar: Rgba,
    pub card: Rgba,
    pub tile: Rgba,
    pub border: Rgba,
    pub accent: Rgba,
    pub light: bool,
}

pub fn theme_preview(theme: Theme) -> ThemePreview {
    let p = palette_of(theme);
    ThemePreview {
        page: rgb(p.night),
        sidebar: rgb(p.deep),
        card: if p.light {
            rgb(DAYLIGHT_CONTROL)
        } else {
            rgb(p.raised)
        },
        tile: rgb(p.raised),
        border: rgb(p.line),
        accent: swatch_accent(p.light),
        light: p.light,
    }
}

fn palette_of(theme: Theme) -> &'static Palette {
    match theme {
        Theme::Night => &NIGHT,
        Theme::Midnight => &MIDNIGHT,
        Theme::Dusk => &DUSK,
        Theme::Daylight => &DAYLIGHT,
    }
}

fn swatch_accent(light: bool) -> Rgba {
    let colors = accent_colors(current_accent());
    rgb(if light { colors.light.0 } else { colors.dark.0 })
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

/// Brand marks (`assets/brand`), embedded under `brand/`. Render with
/// `img(path)` so they keep their colours.
pub const LOGO_SIDEBAR: &str = "brand/ani-gui-sidebar-reversed.svg";
pub const LOGO_SIDEBAR_LIGHT: &str = "brand/ani-gui-sidebar.svg";
/// Height / width of the sidebar lockup's viewBox (753.891 x 240).
pub const LOGO_SIDEBAR_ASPECT: f32 = 240. / 753.891;
pub const LOGO_STACKED: &str = "brand/ani-gui-stacked-reversed.svg";
pub const LOGO_STACKED_LIGHT: &str = "brand/ani-gui-stacked.svg";
/// Height / width of the stacked lockup's viewBox (594.874 x 527.294).
pub const LOGO_STACKED_ASPECT: f32 = 527.294 / 594.874;
/// The tile alone, in colour.
pub const SYMBOL: &str = "brand/ani-gui-symbol.svg";

const BRAND: &[(&str, &[u8])] = &[
    (
        LOGO_SIDEBAR,
        include_bytes!("../../assets/brand/svg/ani-gui-sidebar-reversed.svg"),
    ),
    (
        LOGO_SIDEBAR_LIGHT,
        include_bytes!("../../assets/brand/svg/ani-gui-sidebar.svg"),
    ),
    (
        LOGO_STACKED,
        include_bytes!("../../assets/brand/svg/ani-gui-stacked-reversed.svg"),
    ),
    (
        LOGO_STACKED_LIGHT,
        include_bytes!("../../assets/brand/svg/ani-gui-stacked.svg"),
    ),
    (
        SYMBOL,
        include_bytes!("../../assets/brand/svg/ani-gui-symbol.svg"),
    ),
];

/// The sidebar lockup (with アニグイ) for the current theme.
pub fn sidebar_logo() -> &'static str {
    if is_light() {
        LOGO_SIDEBAR_LIGHT
    } else {
        LOGO_SIDEBAR
    }
}

/// The stacked lockup (with アニグイ) for the current theme.
pub fn stacked_logo() -> &'static str {
    if is_light() {
        LOGO_STACKED_LIGHT
    } else {
        LOGO_STACKED
    }
}

/// UI icons (Lucide, ISC: `assets/icons/ui/LICENSE`), embedded under
/// `icons/<name>.svg`. Render with `components::icon::icon(name)`. To add one,
/// drop the SVG into `assets/icons/ui/` and add its name to `ui_icons!`.
macro_rules! ui_icons {
    ($($name:literal),* $(,)?) => {
        pub const UI_ICONS: &[(&str, &[u8])] = &[
            $((concat!("icons/", $name, ".svg"),
               include_bytes!(concat!("../../assets/icons/ui/", $name, ".svg"))),)*
        ];
    };
}

ui_icons!(
    "arrow-left",
    "bookmark",
    "calendar",
    "captions",
    "captions-off",
    "check",
    "chevron-down",
    "chevron-left",
    "chevron-right",
    "chevron-up",
    "circle-alert",
    "clock",
    "copy",
    "download",
    "external-link",
    "film",
    "flame",
    "folder",
    "gauge",
    "heart",
    "house",
    "info",
    "list",
    "list-video",
    "loader-circle",
    "log-in",
    "log-out",
    "maximize",
    "minimize",
    "minus",
    "palette",
    "pause",
    "play",
    "plus",
    "refresh-cw",
    "rotate-ccw",
    "rotate-cw",
    "search",
    "settings",
    "skip-back",
    "skip-forward",
    "sliders-horizontal",
    "sparkles",
    "square",
    "star",
    "tag",
    "trash-2",
    "trophy",
    "tv",
    "volume-1",
    "volume-2",
    "volume-x",
    "x",
);

/// Images embedded in the binary, so an installed `ani-gui` needs no asset
/// directory. `img(SYMBOL)` goes through GPUI's resource loader, which
/// rasterises SVG in full colour; `svg().path(..)` renders an icon as a
/// tintable mask.
pub struct Assets;

impl Assets {
    fn all() -> impl Iterator<Item = (&'static str, &'static [u8])> {
        BRAND.iter().copied().chain(UI_ICONS.iter().copied())
    }
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        Ok(Assets::all()
            .find(|(p, _)| *p == path)
            .map(|(_, bytes)| Cow::Borrowed(bytes)))
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(Assets::all()
            .filter(|(p, _)| p.starts_with(path))
            .map(|(p, _)| p.into())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assets_serve_embedded_logos() {
        for (path, _) in BRAND {
            let logo = Assets.load(path).expect("load").expect("logo present");
            assert!(logo.starts_with(b"<?xml"), "{path}");
            assert!(
                std::str::from_utf8(&logo).expect("utf8").contains("<svg"),
                "{path}"
            );
        }
    }

    #[test]
    fn every_theme_and_accent_resolves() {
        for theme in Theme::ALL {
            let (page, raised, accent) = theme_swatch(*theme);
            assert_ne!(page, raised, "{theme:?}");
            assert_ne!(page, accent, "{theme:?}");
        }
        for accent in Accent::ALL {
            let colors = accent_colors(*accent);
            assert_ne!(colors.dark.0, colors.dark.2);
            assert_ne!(colors.light.0, colors.light.2);
        }
    }

    #[test]
    fn assets_list_logo_and_miss_unknown_paths() {
        assert!(Assets.load("nope.svg").expect("load").is_none());
        assert!(Assets.list("").expect("list").contains(&SYMBOL.into()));
    }

    #[test]
    fn assets_serve_every_ui_icon() {
        for (path, _) in UI_ICONS {
            let svg = Assets.load(path).expect("load").expect("icon present");
            assert!(
                std::str::from_utf8(&svg).expect("utf8").contains("<svg"),
                "{path}"
            );
        }
        assert!(Assets.load("icons/play.svg").expect("load").is_some());
        assert_eq!(Assets.list("icons/").expect("list").len(), UI_ICONS.len());
        assert_eq!(Assets.list("brand/").expect("list").len(), BRAND.len());
    }

    fn relative_luminance(hex: u32) -> f64 {
        let lin = |c: u32| {
            let s = (c as f64) / 255.0;
            if s <= 0.03928 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin((hex >> 16) & 0xff)
            + 0.7152 * lin((hex >> 8) & 0xff)
            + 0.0722 * lin(hex & 0xff)
    }

    fn contrast(a: u32, b: u32) -> f64 {
        let (l1, l2) = (relative_luminance(a), relative_luminance(b));
        let (hi, lo) = if l1 > l2 { (l1, l2) } else { (l2, l1) };
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn daylight_text_and_accents_meet_wcag_aa() {
        for surface in [DAYLIGHT.night, DAYLIGHT_CONTROL] {
            assert!(contrast(DAYLIGHT.text, surface) >= 12.0);
            assert!(contrast(DAYLIGHT.text_dim, surface) >= 7.0);
            assert!(contrast(DAYLIGHT.text_muted, surface) >= 4.5);
            assert!(contrast(DAYLIGHT.danger, surface) >= 4.5);
        }
        for accent in Accent::ALL {
            let (bg, hover, fg) = accent_colors(*accent).light;
            assert!(contrast(bg, DAYLIGHT_CONTROL) >= 4.5, "{accent:?} on white");
            assert!(contrast(bg, fg) >= 4.5, "{accent:?} button label");
            assert!(contrast(hover, fg) >= 4.5, "{accent:?} hover label");
        }
        let p = theme_preview(Theme::Daylight);
        assert!(p.light);
        assert_ne!(p.page, p.sidebar);
        assert_ne!(p.page, p.card);
    }
}
