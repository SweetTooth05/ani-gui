//! Linux client-side window chrome: slim titlebar, window controls, and
//! floating resize border/shadow/rounding.
//!
//! Compositors without server-side decorations (notably GNOME Mutter on
//! Wayland) give GPUI windows no frame at all unless we draw our own when
//! `window.window_decorations()` reports `Decorations::Client`. Server-side
//! decorations and macOS native windows receive no extra chrome.

use gpui::{AnyElement, App, IntoElement, Window, div, prelude::*};
#[cfg(target_os = "linux")]
use gpui::{
    Bounds, BoxShadow, Corners, CursorStyle, Decorations, Global, HitboxBehavior, MouseButton,
    MouseDownEvent, MouseMoveEvent, Pixels, Point, ResizeEdge, Size, Styled, Tiling, canvas, point,
    px, transparent_black,
};

#[cfg(target_os = "linux")]
use crate::ui::components::icon::icon;
use crate::ui::theme;

#[cfg(target_os = "linux")]
const TITLEBAR_HEIGHT: f32 = 36.;
/// Invisible outer margin reserved for drop shadow and edge resize handles
/// when floating; told to the compositor via `window.set_client_inset`.
#[cfg(target_os = "linux")]
const CLIENT_INSET: f32 = 8.;
#[cfg(target_os = "linux")]
const BORDER_WIDTH: f32 = 1.;
#[cfg(target_os = "linux")]
const CORNER_RADIUS: f32 = 8.;
/// Resize hit strip covers the outer inset plus the 1 px visible border,
/// without reaching into interior controls.
#[cfg(target_os = "linux")]
const RESIZE_BORDER: f32 = CLIENT_INSET + BORDER_WIDTH;
/// Distance along an adjacent untiled edge that counts as a diagonal corner.
#[cfg(target_os = "linux")]
const RESIZE_CORNER: f32 = CLIENT_INSET * 2.;

/// Tracks whether a primary press on the titlebar should start a compositor
/// move once the pointer moves. Deferring `start_window_move` until motion
/// lets double-clicking the titlebar reach `zoom_window` instead of being
/// swallowed by an immediate move grab.
#[cfg(target_os = "linux")]
struct TitlebarDrag(bool);

#[cfg(target_os = "linux")]
impl Global for TitlebarDrag {}

/// Last hovered resize edge so mouse motion only triggers a repaint when the
/// cursor shape needs to change.
#[cfg(target_os = "linux")]
struct CurrentResizeEdge(Option<ResizeEdge>);

#[cfg(target_os = "linux")]
impl Global for CurrentResizeEdge {}

/// Renders the slim top titlebar when client-side decorations are active and
/// the window is not fullscreen. Returns `None` on macOS, under server-side
/// decorations, or while fullscreen.
#[cfg(target_os = "linux")]
pub fn bar(window: &Window) -> Option<AnyElement> {
    if window.is_fullscreen() {
        return None;
    }
    let Decorations::Client { tiling } = window.window_decorations() else {
        return None;
    };
    let controls = window.window_controls();
    let active = window.is_window_active();
    let maximized = window.is_maximized();
    let inner_radii = inner_top_radii(if maximized { Tiling::tiled() } else { tiling });
    let fg = if active {
        theme::text_dim()
    } else {
        theme::text_muted()
    };

    let hover_bg = if theme::is_light() {
        theme::ghost_hover()
    } else {
        theme::raised_hover()
    };
    let close_hover_alpha = if theme::is_light() { 0.14 } else { 0.22 };
    let mut buttons = div().flex().items_center().h_full().flex_none();
    if controls.minimize {
        buttons = buttons.child(
            control_button("window-minimize", "minus", 14., fg, theme::text())
                .hover(move |s| s.bg(hover_bg))
                .on_click(|_, window, _| window.minimize_window()),
        );
    }
    if controls.maximize {
        let (name, size) = if maximized {
            ("copy", 13.)
        } else {
            ("square", 12.)
        };
        buttons = buttons.child(
            control_button("window-maximize", name, size, fg, theme::text())
                .hover(move |s| s.bg(hover_bg))
                .on_click(|_, window, _| window.zoom_window()),
        );
    }
    buttons = buttons.child(
        control_button("window-close", "x", 14., fg, theme::danger())
            .rounded_tr(inner_radii.top_right)
            .hover(move |s| s.bg(gpui::Hsla::from(theme::danger()).opacity(close_hover_alpha)))
            .on_click(|_, window, _| window.remove_window()),
    );

    let title_color: gpui::Hsla = if active {
        theme::text_muted().into()
    } else {
        gpui::Hsla::from(theme::text_muted()).opacity(0.6)
    };

    Some(
        div()
            .id("titlebar")
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .w_full()
            .h(px(TITLEBAR_HEIGHT))
            .bg(theme::deep())
            .border_b_1()
            .border_color(theme::line())
            .rounded_tl(inner_radii.top_left)
            .rounded_tr(inner_radii.top_right)
            .on_mouse_down(MouseButton::Left, |e: &MouseDownEvent, _, cx| {
                if e.click_count == 1 {
                    cx.set_global(TitlebarDrag(true));
                }
            })
            .on_mouse_up(MouseButton::Left, |_, _, cx| {
                cx.set_global(TitlebarDrag(false));
            })
            .on_mouse_down_out(|_, _, cx| {
                cx.set_global(TitlebarDrag(false));
            })
            .on_mouse_move(|_: &MouseMoveEvent, window, cx| {
                if cx.try_global::<TitlebarDrag>().is_some_and(|d| d.0) {
                    cx.set_global(TitlebarDrag(false));
                    window.start_window_move();
                }
            })
            .when(controls.maximize, |bar| {
                bar.on_click(|e, window, _| {
                    if e.click_count() == 2 {
                        window.zoom_window();
                    }
                })
            })
            .when(controls.window_menu, |bar| {
                bar.on_mouse_down(MouseButton::Right, |e: &MouseDownEvent, window, _| {
                    window.show_window_menu(e.position);
                })
            })
            .child(
                div()
                    .pl(px(14.))
                    .text_size(px(12.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(title_color)
                    .child("ani-gui"),
            )
            .child(div().flex_1().h_full())
            .child(buttons)
            .into_any_element(),
    )
}

#[cfg(not(target_os = "linux"))]
pub fn bar(_window: &Window) -> Option<AnyElement> {
    None
}

#[cfg(target_os = "linux")]
fn control_button(
    id: &'static str,
    icon_name: &'static str,
    icon_size: f32,
    fg: gpui::Rgba,
    hover_fg: gpui::Rgba,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .group(id)
        .flex()
        .items_center()
        .justify_center()
        .w(px(44.))
        .h_full()
        .cursor_pointer()
        // Prevent clicks on window controls from initiating a titlebar drag,
        // double-click maximise, or window menu.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .child(
            icon(icon_name)
                .size(px(icon_size))
                .text_color(fg)
                .group_hover(id, move |s| s.text_color(hover_fg)),
        )
}

/// Wraps the root view in client-side window decorations (shadow, rounded
/// corners, border, and resize handles) when floating under `Decorations::Client`.
/// Under `Decorations::Server`, fullscreen, or non-Linux platforms, returns
/// `content` unchanged.
#[cfg(target_os = "linux")]
pub fn frame(content: impl IntoElement, window: &mut Window, _cx: &mut App) -> AnyElement {
    let decorations = window.window_decorations();
    let Decorations::Client { tiling } = decorations else {
        window.set_client_inset(px(0.));
        return div()
            .size_full()
            .bg(theme::night())
            .child(content)
            .into_any_element();
    };

    let tiling = if window.is_fullscreen() || window.is_maximized() {
        Tiling::tiled()
    } else {
        tiling
    };
    let all_tiled = is_all_tiled(tiling);

    window.set_client_inset(if all_tiled { px(0.) } else { px(CLIENT_INSET) });

    let radii = corner_radii_for(tiling);
    let inner = with_corner_radii(
        div()
            .size_full()
            .overflow_hidden()
            .bg(theme::night())
            .cursor(CursorStyle::Arrow)
            .border_color(theme::line())
            .when(!tiling.top, |d| d.border_t(px(BORDER_WIDTH)))
            .when(!tiling.bottom, |d| d.border_b(px(BORDER_WIDTH)))
            .when(!tiling.left, |d| d.border_l(px(BORDER_WIDTH)))
            .when(!tiling.right, |d| d.border_r(px(BORDER_WIDTH)))
            .when(!tiling.is_tiled(), |d| {
                d.shadow(vec![if theme::is_light() {
                    BoxShadow {
                        color: gpui::black().opacity(0.16),
                        offset: point(px(0.), px(3.)),
                        blur_radius: px(12.),
                        spread_radius: px(0.),
                    }
                } else {
                    BoxShadow {
                        color: gpui::black().opacity(0.4),
                        offset: point(px(0.), px(2.)),
                        blur_radius: px(6.),
                        spread_radius: px(0.),
                    }
                }])
            }),
        radii,
    )
    .child(content);

    let mut backdrop = div()
        .id("window-backdrop")
        .relative()
        .size_full()
        .bg(transparent_black())
        .when(!tiling.top, |d| d.pt(px(CLIENT_INSET)))
        .when(!tiling.bottom, |d| d.pb(px(CLIENT_INSET)))
        .when(!tiling.left, |d| d.pl(px(CLIENT_INSET)))
        .when(!tiling.right, |d| d.pr(px(CLIENT_INSET)));

    if !all_tiled {
        backdrop = backdrop
            .on_mouse_move(move |e: &MouseMoveEvent, window, cx| {
                let edge = resize_edge(e.position, window.viewport_size(), tiling);
                let prev = cx.try_global::<CurrentResizeEdge>().and_then(|g| g.0);
                if edge != prev {
                    cx.set_global(CurrentResizeEdge(edge));
                    window.refresh();
                }
            })
            .on_mouse_down(MouseButton::Left, move |e: &MouseDownEvent, window, cx| {
                if let Some(edge) = resize_edge(e.position, window.viewport_size(), tiling) {
                    cx.stop_propagation();
                    window.start_window_resize(edge);
                }
            });
    }

    backdrop = backdrop.child(inner);

    if !all_tiled {
        backdrop = backdrop.child(
            canvas(
                |_bounds, window, _| {
                    window.insert_hitbox(
                        Bounds::new(Point::default(), window.viewport_size()),
                        HitboxBehavior::Normal,
                    )
                },
                move |_bounds, hitbox, window, cx| {
                    let edge = resize_edge(window.mouse_position(), window.viewport_size(), tiling);
                    cx.set_global(CurrentResizeEdge(edge));
                    if let Some(edge) = edge {
                        window.set_cursor_style(resize_cursor(edge), &hitbox);
                    }
                },
            )
            .absolute()
            .inset_0(),
        );
    }

    backdrop.into_any_element()
}

#[cfg(not(target_os = "linux"))]
pub fn frame(content: impl IntoElement, _window: &mut Window, _cx: &mut App) -> AnyElement {
    div()
        .size_full()
        .bg(theme::night())
        .child(content)
        .into_any_element()
}

#[cfg(target_os = "linux")]
fn is_all_tiled(tiling: Tiling) -> bool {
    tiling.top && tiling.bottom && tiling.left && tiling.right
}

#[cfg(target_os = "linux")]
fn corner_radii_for(tiling: Tiling) -> Corners<Pixels> {
    let radius = |floating: bool| if floating { px(CORNER_RADIUS) } else { px(0.) };
    Corners {
        top_left: radius(!tiling.top && !tiling.left),
        top_right: radius(!tiling.top && !tiling.right),
        bottom_right: radius(!tiling.bottom && !tiling.right),
        bottom_left: radius(!tiling.bottom && !tiling.left),
    }
}

#[cfg(target_os = "linux")]
fn inner_top_radii(tiling: Tiling) -> Corners<Pixels> {
    let inner = (CORNER_RADIUS - BORDER_WIDTH).max(0.);
    let radius = |floating: bool| if floating { px(inner) } else { px(0.) };
    Corners {
        top_left: radius(!tiling.top && !tiling.left),
        top_right: radius(!tiling.top && !tiling.right),
        bottom_right: px(0.),
        bottom_left: px(0.),
    }
}

#[cfg(target_os = "linux")]
fn with_corner_radii<E: Styled>(element: E, radii: Corners<Pixels>) -> E {
    element
        .rounded_tl(radii.top_left)
        .rounded_tr(radii.top_right)
        .rounded_br(radii.bottom_right)
        .rounded_bl(radii.bottom_left)
}

#[cfg(target_os = "linux")]
fn resize_cursor(edge: ResizeEdge) -> CursorStyle {
    match edge {
        ResizeEdge::Top | ResizeEdge::Bottom => CursorStyle::ResizeUpDown,
        ResizeEdge::Left | ResizeEdge::Right => CursorStyle::ResizeLeftRight,
        ResizeEdge::TopLeft | ResizeEdge::BottomRight => CursorStyle::ResizeUpLeftDownRight,
        ResizeEdge::TopRight | ResizeEdge::BottomLeft => CursorStyle::ResizeUpRightDownLeft,
    }
}

/// Determines which window edge or corner `pos` hits within the resize border.
/// Tiled edges are excluded so maximised/fullscreen/snapped edges never hijack
/// pointer events.
#[cfg(target_os = "linux")]
fn resize_edge(pos: Point<Pixels>, viewport: Size<Pixels>, tiling: Tiling) -> Option<ResizeEdge> {
    let border = px(RESIZE_BORDER);
    let corner = px(RESIZE_CORNER);

    let on_top = !tiling.top && pos.y <= border;
    let on_bottom = !tiling.bottom && pos.y >= viewport.height - border;
    let on_left = !tiling.left && pos.x <= border;
    let on_right = !tiling.right && pos.x >= viewport.width - border;

    if !(on_top || on_bottom || on_left || on_right) {
        return None;
    }

    let near_top = !tiling.top && pos.y <= corner;
    let near_bottom = !tiling.bottom && pos.y >= viewport.height - corner;
    let near_left = !tiling.left && pos.x <= corner;
    let near_right = !tiling.right && pos.x >= viewport.width - corner;

    match (near_top, near_bottom, near_left, near_right) {
        (true, _, true, _) => Some(ResizeEdge::TopLeft),
        (true, _, _, true) => Some(ResizeEdge::TopRight),
        (_, true, true, _) => Some(ResizeEdge::BottomLeft),
        (_, true, _, true) => Some(ResizeEdge::BottomRight),
        _ if on_top => Some(ResizeEdge::Top),
        _ if on_bottom => Some(ResizeEdge::Bottom),
        _ if on_left => Some(ResizeEdge::Left),
        _ if on_right => Some(ResizeEdge::Right),
        _ => None,
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use gpui::size;

    #[test]
    fn resize_edge_hits_all_edges_and_corners_when_floating() {
        let vp = size(px(1200.), px(800.));
        let floating = Tiling::default();

        assert_eq!(resize_edge(point(px(600.), px(400.)), vp, floating), None);
        // Interior near corner (outside the 9 px border strip) stays free for buttons.
        assert_eq!(resize_edge(point(px(1188.), px(12.)), vp, floating), None);

        assert_eq!(
            resize_edge(point(px(600.), px(4.)), vp, floating),
            Some(ResizeEdge::Top)
        );
        assert_eq!(
            resize_edge(point(px(600.), px(796.)), vp, floating),
            Some(ResizeEdge::Bottom)
        );
        assert_eq!(
            resize_edge(point(px(4.), px(400.)), vp, floating),
            Some(ResizeEdge::Left)
        );
        assert_eq!(
            resize_edge(point(px(1196.), px(400.)), vp, floating),
            Some(ResizeEdge::Right)
        );
        assert_eq!(
            resize_edge(point(px(4.), px(12.)), vp, floating),
            Some(ResizeEdge::TopLeft)
        );
        assert_eq!(
            resize_edge(point(px(1190.), px(4.)), vp, floating),
            Some(ResizeEdge::TopRight)
        );
        assert_eq!(
            resize_edge(point(px(12.), px(796.)), vp, floating),
            Some(ResizeEdge::BottomLeft)
        );
        assert_eq!(
            resize_edge(point(px(1196.), px(790.)), vp, floating),
            Some(ResizeEdge::BottomRight)
        );
    }

    #[test]
    fn resize_edge_ignores_tiled_and_maximised_edges() {
        let vp = size(px(1200.), px(800.));
        let tiled = Tiling::tiled();
        assert_eq!(resize_edge(point(px(0.), px(0.)), vp, tiled), None);
        assert_eq!(resize_edge(point(px(1199.), px(400.)), vp, tiled), None);

        // Snapped to the left half: only the right edge remains resizable.
        let left_half = Tiling {
            top: true,
            bottom: true,
            left: true,
            right: false,
        };
        assert_eq!(resize_edge(point(px(2.), px(400.)), vp, left_half), None);
        assert_eq!(
            resize_edge(point(px(1196.), px(2.)), vp, left_half),
            Some(ResizeEdge::Right)
        );
    }

    #[test]
    fn corners_round_only_when_adjacent_edges_float() {
        let floating = corner_radii_for(Tiling::default());
        assert_eq!(floating.top_left, px(CORNER_RADIUS));
        assert_eq!(floating.top_right, px(CORNER_RADIUS));
        assert_eq!(floating.bottom_left, px(CORNER_RADIUS));
        assert_eq!(floating.bottom_right, px(CORNER_RADIUS));

        let tiled = corner_radii_for(Tiling::tiled());
        assert_eq!(tiled.top_left, px(0.));
        assert_eq!(tiled.top_right, px(0.));
        assert_eq!(tiled.bottom_left, px(0.));
        assert_eq!(tiled.bottom_right, px(0.));

        let top_tiled = corner_radii_for(Tiling {
            top: true,
            ..Tiling::default()
        });
        assert_eq!(top_tiled.top_left, px(0.));
        assert_eq!(top_tiled.top_right, px(0.));
        assert_eq!(top_tiled.bottom_left, px(CORNER_RADIUS));
        assert_eq!(top_tiled.bottom_right, px(CORNER_RADIUS));
    }
}
