use gpui::{Context, FontWeight, IntoElement, div, prelude::*, px};

use crate::app::{Root, Route};
use crate::ui::theme;

const ITEMS: [(&str, Route); 6] = [
    ("Home", Route::Home),
    ("Search", Route::Search),
    ("My List", Route::MyList),
    ("Calendar", Route::Calendar),
    ("Downloads", Route::Downloads),
    ("Settings", Route::Settings),
];

/// The sidebar entry a route belongs to (Show and Player pages have none).
fn section(route: &Route) -> Option<&'static str> {
    ITEMS
        .iter()
        .find(|(_, r)| r == route)
        .map(|(label, _)| *label)
}

pub fn sidebar(active: &Route, cx: &mut Context<Root>) -> impl IntoElement {
    let active = section(active);
    div()
        .flex()
        .flex_col()
        .flex_none()
        .w(px(theme::SIDEBAR_WIDTH))
        .h_full()
        .bg(theme::deep())
        .border_r_1()
        .border_color(theme::line())
        .pt(px(28.))
        .px(px(14.))
        .gap(px(2.))
        .child(
            div()
                .px(px(14.))
                .pb(px(28.))
                .font_family(theme::DISPLAY_FONT)
                .font_weight(FontWeight::BOLD)
                .text_size(px(30.))
                .line_height(px(30.))
                .text_color(theme::text())
                .child("ani-gui"),
        )
        .children(ITEMS.into_iter().map(|(label, route)| {
            let selected = active == Some(label);
            div()
                .id(label)
                .relative()
                .flex()
                .items_center()
                .h(px(42.))
                .px(px(14.))
                .rounded(px(8.))
                .text_size(px(15.))
                .font_weight(if selected {
                    FontWeight::SEMIBOLD
                } else {
                    FontWeight::MEDIUM
                })
                .when(selected, |item| {
                    item.bg(theme::raised()).text_color(theme::text()).child(
                        div()
                            .absolute()
                            .left(px(0.))
                            .top(px(11.))
                            .h(px(20.))
                            .w(px(3.))
                            .rounded(px(2.))
                            .bg(theme::accent()),
                    )
                })
                .when(!selected, |item| {
                    item.text_color(theme::text_dim())
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme::text()).bg(theme::wash()))
                })
                .on_click(cx.listener(move |root, _, _, cx| root.navigate(route.clone(), cx)))
                .child(label)
        }))
}
