use gpui::{
    AnyElement, ElementId, FontWeight, IntoElement, ScrollHandle, SharedString, div, point,
    prelude::*, px,
};

use crate::ui::theme;

/// A titled row of cards that scrolls sideways: by touchpad, or a page at a
/// time with the ‹ › buttons (the mouse wheel keeps scrolling the page).
pub fn row(
    id: impl Into<SharedString>,
    title: impl Into<SharedString>,
    scroll: &ScrollHandle,
    cards: impl IntoIterator<Item = AnyElement>,
) -> impl IntoElement {
    let id: SharedString = id.into();
    let pager = |label: &'static str, dir: f32| {
        let scroll = scroll.clone();
        div()
            .id(ElementId::Name(format!("{id}-{label}").into()))
            .flex()
            .items_center()
            .justify_center()
            .size(px(32.))
            .rounded_full()
            .text_size(px(20.))
            .text_color(theme::text_dim())
            .cursor_pointer()
            .hover(|s| s.bg(theme::ghost_hover()).text_color(theme::text()))
            .child(label)
            .on_click(move |_, window, _| {
                let page = scroll.bounds().size.width * 0.8;
                let max = scroll.max_offset().width;
                let offset = scroll.offset();
                let x = (offset.x - page * dir).clamp(-max, px(0.));
                scroll.set_offset(point(x, offset.y));
                window.refresh();
            })
    };
    let mut strip = div()
        .id(ElementId::Name(id.clone()))
        .flex()
        .gap(px(18.))
        .px(theme::gutter())
        .pb(px(4.))
        .overflow_x_scroll()
        .track_scroll(scroll)
        .children(cards);
    strip.style().restrict_scroll_to_axis = Some(true);
    div()
        .flex()
        .flex_col()
        .gap(px(14.))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .px(theme::gutter())
                .child(
                    div()
                        .font_family(theme::DISPLAY_FONT)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_size(px(theme::SECTION))
                        .text_color(theme::text())
                        .child(title.into()),
                )
                .child(
                    div()
                        .flex()
                        .gap_1()
                        .child(pager("‹", -1.))
                        .child(pager("›", 1.)),
                ),
        )
        .child(strip)
}
