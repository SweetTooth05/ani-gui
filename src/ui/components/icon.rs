//! Monochrome UI icons from `theme::UI_ICONS`, tinted with `text_color`, and
//! the colour brand mark.

use gpui::{Img, ObjectFit, Styled, StyledImage, Svg, img, px, svg};

use crate::ui::theme;

/// The icon `name` (an entry of `ui_icons!` in `theme.rs`, e.g. "play") at
/// 20 px in the body text colour. Chain `.size(..)` / `.text_color(..)` to
/// change either.
pub fn icon(name: &'static str) -> Svg {
    debug_assert!(
        theme::UI_ICONS.iter().any(|(p, _)| p
            .strip_prefix("icons/")
            .and_then(|p| p.strip_suffix(".svg"))
            == Some(name)),
        "unknown icon {name}"
    );
    svg()
        .path(format!("icons/{name}.svg"))
        .size(px(20.))
        .flex_none()
        .text_color(theme::text())
}

/// The ani-gui tile (the ア Play mark) in full colour, `size` px square:
/// for empty and error states.
pub fn brand_mark(size: f32) -> Img {
    img(theme::SYMBOL)
        .size(px(size))
        .flex_none()
        .object_fit(ObjectFit::Contain)
}
