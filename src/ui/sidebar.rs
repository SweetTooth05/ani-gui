use std::time::Duration;

use gpui::{
    Animation, AnimationExt, AnyElement, Context, Entity, FontWeight, Global, IntoElement,
    ObjectFit, Transformation, Window, div, img, percentage, prelude::*, px, relative,
};

use crate::app::{Root, Route};
use crate::ui::components::icon::icon;
use crate::ui::theme;
use crate::updater::{self, SidebarTone, UpdateAction};

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

/// Self-observing footer widget so updater progress repaints only this control.
struct SidebarUpdateControl;

impl SidebarUpdateControl {
    fn new(cx: &mut Context<Self>) -> Self {
        if let Some(updates) = updater::model(cx) {
            cx.observe(&updates, |_, _, cx| cx.notify()).detach();
        }
        Self
    }
}

struct SidebarUpdateHost(Entity<SidebarUpdateControl>);

impl Global for SidebarUpdateHost {}

fn update_control(cx: &mut Context<Root>) -> Entity<SidebarUpdateControl> {
    if let Some(host) = cx.try_global::<SidebarUpdateHost>() {
        return host.0.clone();
    }
    let entity = cx.new(SidebarUpdateControl::new);
    cx.set_global(SidebarUpdateHost(entity.clone()));
    entity
}

impl Render for SidebarUpdateControl {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(updates) = updater::model(cx) else {
            return div().into_any_element();
        };
        let state = updater::sidebar_state(updates.read(cx));
        let action = state.action.clone();
        let control: AnyElement = match state.tone {
            SidebarTone::Quiet => div()
                .id("sidebar-update-check")
                .w_full()
                .px(px(10.))
                .py(px(8.))
                .rounded(px(8.))
                .text_size(px(12.))
                .text_color(theme::text_muted())
                .cursor_pointer()
                .hover(|s| s.bg(theme::wash()).text_color(theme::text_dim()))
                .when_some(action, |el, act| {
                    el.on_click(move |_, _, cx| updater::perform(act.clone(), cx))
                })
                .child(div().truncate().child(state.label))
                .into_any_element(),
            SidebarTone::Busy => {
                let spinner = icon("loader-circle")
                    .size(px(14.))
                    .text_color(theme::accent())
                    .with_animation(
                        "sidebar-update-spinner",
                        Animation::new(Duration::from_millis(900)).repeat(),
                        |svg, delta| {
                            svg.with_transformation(Transformation::rotate(percentage(delta)))
                        },
                    );
                div()
                    .id("sidebar-update-busy")
                    .w_full()
                    .px(px(10.))
                    .py(px(8.))
                    .rounded(px(8.))
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .min_w_0()
                            .child(spinner)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(12.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme::text_dim())
                                    .truncate()
                                    .child(state.label),
                            ),
                    )
                    .children(state.progress.map(|frac| {
                        div()
                            .h(px(3.))
                            .w_full()
                            .rounded_full()
                            .bg(theme::raised())
                            .overflow_hidden()
                            .child(
                                div()
                                    .h_full()
                                    .w(relative(frac.clamp(0.0, 1.0)))
                                    .bg(theme::accent()),
                            )
                    }))
                    .into_any_element()
            }
            SidebarTone::Ready => {
                let icon_name = match &action {
                    Some(UpdateAction::OpenRelease { .. }) => "external-link",
                    _ => "download",
                };
                div()
                    .id("sidebar-update-ready")
                    .w_full()
                    .px(px(10.))
                    .py(px(9.))
                    .rounded(px(8.))
                    .bg(theme::raised())
                    .border_1()
                    .border_color(theme::line())
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::raised_hover()))
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .when_some(action, |el, act| {
                        el.on_click(move |_, _, cx| updater::perform(act.clone(), cx))
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(7.))
                            .min_w_0()
                            .child(icon(icon_name).size(px(14.)).text_color(theme::accent()))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(theme::SMALL))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::text())
                                    .truncate()
                                    .child(state.label),
                            ),
                    )
                    .children(state.detail.map(|detail| {
                        div()
                            .text_size(px(11.5))
                            .text_color(theme::text_muted())
                            .truncate()
                            .child(detail)
                    }))
                    .into_any_element()
            }
            SidebarTone::Accent => div()
                .flex()
                .flex_col()
                .gap(px(5.))
                .w_full()
                .children(state.detail.map(|detail| {
                    div()
                        .px(px(2.))
                        .text_size(px(11.5))
                        .text_color(theme::text_muted())
                        .truncate()
                        .child(detail)
                }))
                .child(
                    div()
                        .id("sidebar-update-restart")
                        .w_full()
                        .px(px(10.))
                        .py(px(9.))
                        .rounded(px(8.))
                        .bg(theme::accent())
                        .text_color(theme::on_accent())
                        .cursor_pointer()
                        .hover(|s| s.bg(theme::accent_hover()))
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap(px(7.))
                        .when_some(action, |el, act| {
                            el.on_click(move |_, _, cx| updater::perform(act.clone(), cx))
                        })
                        .child(
                            icon("rotate-cw")
                                .size(px(14.))
                                .text_color(theme::on_accent()),
                        )
                        .child(
                            div()
                                .text_size(px(theme::SMALL))
                                .font_weight(FontWeight::SEMIBOLD)
                                .whitespace_nowrap()
                                .child(state.label),
                        ),
                )
                .into_any_element(),
            SidebarTone::Error => div()
                .id("sidebar-update-error")
                .w_full()
                .px(px(10.))
                .py(px(8.))
                .rounded(px(8.))
                .bg(theme::wash())
                .cursor_pointer()
                .hover(|s| s.bg(theme::raised()))
                .flex()
                .flex_col()
                .gap(px(2.))
                .when_some(action, |el, act| {
                    el.on_click(move |_, _, cx| updater::perform(act.clone(), cx))
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(7.))
                        .min_w_0()
                        .child(
                            icon("circle-alert")
                                .size(px(14.))
                                .text_color(theme::danger()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(theme::SMALL))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme::danger())
                                .truncate()
                                .child(state.label),
                        ),
                )
                .children(state.detail.map(|detail| {
                    div()
                        .text_size(px(11.5))
                        .text_color(theme::text_muted())
                        .truncate()
                        .child(detail)
                }))
                .into_any_element(),
        };

        div()
            .pt(px(10.))
            .pb(px(14.))
            .border_t_1()
            .border_color(theme::line())
            .child(control)
            .into_any_element()
    }
}

pub fn sidebar(active: &Route, cx: &mut Context<Root>) -> impl IntoElement {
    let active = section(active);
    let footer = update_control(cx);
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
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
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
                        .on_click(
                            cx.listener(move |root, _, _, cx| root.navigate(route.clone(), cx)),
                        )
                        .child(label)
                })),
        )
        .child(footer)
}
