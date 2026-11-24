use gpui::{Context, FontWeight, IntoElement, ObjectFit, div, img, prelude::*, px};

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

/// Logo width: the sidebar minus its padding and the nav items' inset, so the
/// lockup lines up with the labels below it.
const LOGO_W: f32 = theme::SIDEBAR_WIDTH - 4. * 14.;
/// Height / width of `assets/brand/svg/ani-gui-compact-reversed.svg` (882 x 240).
const LOGO_ASPECT: f32 = 240. / 882.;

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
            div().px(px(14.)).pb(px(28.)).child(
                img(theme::LOGO)
                    .w(px(LOGO_W))
                    .h(px(LOGO_W * LOGO_ASPECT))
                    .object_fit(ObjectFit::Contain),
            ),
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
