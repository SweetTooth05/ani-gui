use std::rc::Rc;

use gpui::{
    AnyElement, App, ClickEvent, FontWeight, IntoElement, Window, div, prelude::*, px, relative,
};

use crate::ui::components::button::{ButtonKind, button};
use crate::ui::theme;

pub type OnClick = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// "Which one is it?": ani-cli's results for `title` when none matched it.
/// `on_choose` gets the picked candidate; `on_scrim`, if any, runs on a
/// click outside the dialog.
pub fn pick_dialog(
    title: &str,
    candidates: &[String],
    on_choose: impl Fn(&str, &mut Window, &mut App) + 'static,
    on_cancel: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_scrim: Option<OnClick>,
) -> AnyElement {
    let on_choose = Rc::new(on_choose);
    let scrim = div()
        .id("pick-scrim")
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(theme::scrim());
    let scrim = match on_scrim {
        Some(f) => scrim.on_click(f),
        None => scrim,
    };
    scrim
        .child(
            div()
                .id("pick")
                .w(px(560.))
                .max_h(px(560.))
                .flex()
                .flex_col()
                .gap(px(16.))
                .p(px(28.))
                .rounded(px(16.))
                .bg(theme::night())
                .border_1()
                .border_color(theme::line())
                .shadow_lg()
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .font_family(theme::DISPLAY_FONT)
                        .font_weight(FontWeight::BOLD)
                        .text_size(px(34.))
                        .child("Which one is it?"),
                )
                .child(
                    div()
                        .text_size(px(theme::BODY))
                        .line_height(relative(1.45))
                        .text_color(theme::text_dim())
                        .child(format!(
                            "ani-cli's search didn't find an exact match for \u{201c}{title}\u{201d}. \
                             Pick the matching show and it will be used from now on."
                        )),
                )
                .child(
                    div()
                        .id("pick-list")
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .overflow_y_scroll()
                        .children(candidates.iter().enumerate().map(|(i, c)| {
                            let (chosen, on_choose) = (c.clone(), on_choose.clone());
                            div()
                                .id(("candidate", i))
                                .px(px(16.))
                                .py(px(12.))
                                .rounded(px(10.))
                                .bg(theme::raised())
                                .font_weight(FontWeight::MEDIUM)
                                .cursor_pointer()
                                .hover(|s| s.bg(theme::raised_hover()))
                                .on_click(move |_, window, cx| on_choose(&chosen, window, cx))
                                .child(c.clone())
                        })),
                )
                .child(
                    div().flex().justify_end().child(
                        button("pick-cancel", "Cancel")
                            .kind(ButtonKind::Ghost)
                            .on_click(on_cancel),
                    ),
                ),
        )
        .into_any_element()
}
