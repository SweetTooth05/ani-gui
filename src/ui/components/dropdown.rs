//! Compact dropdown button with an anchored, scrollable popover menu.
//!
//! Supports single-choice rows (closing the menu on pick), multi-select rows
//! (green `+` mark, staying open), tri-state include/exclude rows (click row
//! to cycle or use the explicit `+` / `−` buttons, staying open), and small
//! section headings.

use std::rc::Rc;

use gpui::{
    AnyElement, App, ClickEvent, Corner, Edges, ElementId, FocusHandle, FontWeight, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, SharedString, Window, anchored, deferred, div,
    point, prelude::*, px,
};

use crate::ui::components::chip::{negative, negative_alpha, positive, positive_alpha};
use crate::ui::components::icon::icon;
use crate::ui::theme;

pub const TRIGGER_HEIGHT: f32 = 32.;
pub const MENU_MAX_HEIGHT: f32 = 360.;
pub const DEFAULT_MENU_WIDTH: f32 = 220.;

type OnClick = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;
type OnOpen = Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App) + 'static>;
type OnClose = Rc<dyn Fn(&mut Window, &mut App) + 'static>;

/// Include/exclude state for tri-state filter rows (genres and tags).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriState {
    Off,
    Included,
    Excluded,
}

enum MenuItem {
    Heading(SharedString),
    Single {
        label: SharedString,
        selected: bool,
        on_pick: OnClick,
    },
    Multi {
        label: SharedString,
        selected: bool,
        on_toggle: OnClick,
    },
    Tri {
        label: SharedString,
        state: TriState,
        on_cycle: OnClick,
        on_include: OnClick,
        on_exclude: OnClick,
    },
}

#[derive(IntoElement)]
pub struct Dropdown {
    id: ElementId,
    label: SharedString,
    summary: Option<SharedString>,
    active: bool,
    open: bool,
    menu_width: f32,
    focus_handle: Option<FocusHandle>,
    on_open: Option<OnOpen>,
    on_close: Option<OnClose>,
    items: Vec<MenuItem>,
}

/// A dropdown button labelled `label`, with an optional anchored menu when open.
pub fn dropdown(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Dropdown {
    Dropdown {
        id: id.into(),
        label: label.into(),
        summary: None,
        active: false,
        open: false,
        menu_width: DEFAULT_MENU_WIDTH,
        focus_handle: None,
        on_open: None,
        on_close: None,
        items: Vec::new(),
    }
}

impl Dropdown {
    /// Summary shown after the label (e.g. `Genres · Action +1`).
    pub fn summary(mut self, summary: Option<impl Into<SharedString>>) -> Self {
        self.summary = summary.map(Into::into);
        self
    }

    /// Highlights the trigger button as an active narrowing filter.
    pub fn active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    /// Whether the popover menu is currently open.
    pub fn open(mut self, open: bool) -> Self {
        self.open = open;
        self
    }

    /// Width of the popover menu in pixels.
    pub fn menu_width(mut self, width: f32) -> Self {
        self.menu_width = width;
        self
    }

    /// Focus handle attached to the open menu so pressing Escape closes it.
    pub fn focus_handle(mut self, handle: FocusHandle) -> Self {
        self.focus_handle = Some(handle);
        self
    }

    /// Called on mouse-down when the trigger is pressed while closed.
    pub fn on_open(mut self, f: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_open = Some(Box::new(f));
        self
    }

    /// Called when the menu should close (outside click, Escape, or single-choice pick).
    pub fn on_close(mut self, f: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_close = Some(Rc::new(f));
        self
    }

    /// Small group heading inside the menu.
    pub fn heading(mut self, title: impl Into<SharedString>) -> Self {
        self.items.push(MenuItem::Heading(title.into()));
        self
    }

    /// Single-choice row; picking invokes `on_pick` and closes the menu.
    pub fn single(
        mut self,
        label: impl Into<SharedString>,
        selected: bool,
        on_pick: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.items.push(MenuItem::Single {
            label: label.into(),
            selected,
            on_pick: Box::new(on_pick),
        });
        self
    }

    /// Multi-select row with a green `+` mark when selected; menu stays open.
    pub fn multi(
        mut self,
        label: impl Into<SharedString>,
        selected: bool,
        on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.items.push(MenuItem::Multi {
            label: label.into(),
            selected,
            on_toggle: Box::new(on_toggle),
        });
        self
    }

    /// Tri-state include/exclude row: clicking the row cycles; dedicated `+` / `−`
    /// controls on the right toggle include or exclude directly. Menu stays open.
    pub fn tri(
        mut self,
        label: impl Into<SharedString>,
        state: TriState,
        on_cycle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        on_include: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        on_exclude: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.items.push(MenuItem::Tri {
            label: label.into(),
            state,
            on_cycle: Box::new(on_cycle),
            on_include: Box::new(on_include),
            on_exclude: Box::new(on_exclude),
        });
        self
    }
}

fn child_id(parent: &ElementId, suffix: &'static str) -> ElementId {
    ElementId::NamedChild(Box::new(parent.clone()), suffix.into())
}

fn render_item(
    index: usize,
    item: MenuItem,
    on_close: Option<OnClose>,
    light: bool,
) -> Vec<AnyElement> {
    match item {
        MenuItem::Heading(title) => {
            let mut els = Vec::new();
            if index > 0 {
                els.push(
                    div()
                        .flex_none()
                        .h(px(1.))
                        .my(px(4.))
                        .mx(px(6.))
                        .bg(theme::line())
                        .into_any_element(),
                );
            }
            els.push(
                div()
                    .flex_none()
                    .px(px(10.))
                    .pt(px(if index == 0 { 4. } else { 6. }))
                    .pb(px(4.))
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text_muted())
                    .child(title)
                    .into_any_element(),
            );
            els
        }
        MenuItem::Single {
            label,
            selected,
            on_pick,
        } => {
            let mut row = div()
                .id(("dd-single", index))
                .flex()
                .flex_none()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .h(px(32.))
                .px(px(10.))
                .rounded(px(7.))
                .text_size(px(theme::SMALL))
                .whitespace_nowrap()
                .cursor_pointer()
                .on_click(move |ev, window, cx| {
                    on_pick(ev, window, cx);
                    if let Some(close) = &on_close {
                        close(window, cx);
                    }
                })
                .child(div().min_w_0().overflow_hidden().child(label));
            row = if selected {
                row.bg(theme::accent_alpha(if light { 0.14 } else { 0.16 }))
                    .text_color(theme::text())
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(icon("check").size(px(14.)).text_color(theme::accent()))
            } else {
                row.text_color(theme::text_dim())
                    .hover(|s| s.bg(theme::raised_hover()).text_color(theme::text()))
            };
            vec![row.into_any_element()]
        }
        MenuItem::Multi {
            label,
            selected,
            on_toggle,
        } => {
            let mut row = div()
                .id(("dd-multi", index))
                .flex()
                .flex_none()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .h(px(32.))
                .px(px(10.))
                .rounded(px(7.))
                .text_size(px(theme::SMALL))
                .whitespace_nowrap()
                .cursor_pointer()
                .on_click(on_toggle)
                .child(div().min_w_0().overflow_hidden().child(label));
            row = if selected {
                let bg = positive_alpha(if light { 0.14 } else { 0.15 });
                let hover_bg = positive_alpha(if light { 0.20 } else { 0.22 });
                row.bg(bg)
                    .text_color(positive())
                    .font_weight(FontWeight::SEMIBOLD)
                    .hover(move |s| s.bg(hover_bg))
                    .child(icon("plus").size(px(14.)).text_color(positive()))
            } else {
                row.text_color(theme::text_dim())
                    .hover(|s| s.bg(theme::raised_hover()).text_color(theme::text()))
            };
            vec![row.into_any_element()]
        }
        MenuItem::Tri {
            label,
            state,
            on_cycle,
            on_include,
            on_exclude,
        } => {
            let inc_group: SharedString = format!("dd-tri-inc-{index}").into();
            let exc_group: SharedString = format!("dd-tri-exc-{index}").into();

            let mut inc_btn = div()
                .id(("dd-tri-inc", index))
                .group(inc_group.clone())
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .w(px(22.))
                .h(px(22.))
                .rounded(px(5.))
                .border_1()
                .cursor_pointer()
                .on_click(on_include);
            inc_btn = if state == TriState::Included {
                inc_btn
                    .bg(positive_alpha(if light { 0.22 } else { 0.24 }))
                    .border_color(positive_alpha(0.55))
                    .child(icon("plus").size(px(13.)).text_color(positive()))
            } else {
                let hover_bg = positive_alpha(if light { 0.16 } else { 0.18 });
                let pos = positive();
                inc_btn
                    .border_color(gpui::transparent_black())
                    .hover(move |s| s.bg(hover_bg))
                    .child(
                        icon("plus")
                            .size(px(13.))
                            .text_color(theme::text_muted())
                            .group_hover(inc_group, move |s| s.text_color(pos)),
                    )
            };

            let mut exc_btn = div()
                .id(("dd-tri-exc", index))
                .group(exc_group.clone())
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .w(px(22.))
                .h(px(22.))
                .rounded(px(5.))
                .border_1()
                .cursor_pointer()
                .on_click(on_exclude);
            exc_btn = if state == TriState::Excluded {
                exc_btn
                    .bg(negative_alpha(if light { 0.22 } else { 0.24 }))
                    .border_color(negative_alpha(0.55))
                    .child(icon("minus").size(px(13.)).text_color(negative()))
            } else {
                let hover_bg = negative_alpha(if light { 0.16 } else { 0.18 });
                let neg = negative();
                exc_btn
                    .border_color(gpui::transparent_black())
                    .hover(move |s| s.bg(hover_bg))
                    .child(
                        icon("minus")
                            .size(px(13.))
                            .text_color(theme::text_muted())
                            .group_hover(exc_group, move |s| s.text_color(neg)),
                    )
            };

            let label_area = div()
                .id(("dd-tri-row", index))
                .flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .items_center()
                .overflow_hidden()
                .whitespace_nowrap()
                .cursor_pointer()
                .on_click(on_cycle)
                .child(label);

            let mut row = div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(6.))
                .h(px(32.))
                .pl(px(10.))
                .pr(px(5.))
                .rounded(px(7.))
                .text_size(px(theme::SMALL));
            row = match state {
                TriState::Off => row
                    .text_color(theme::text_dim())
                    .hover(|s| s.bg(theme::raised_hover()).text_color(theme::text())),
                TriState::Included => {
                    let bg = positive_alpha(if light { 0.14 } else { 0.15 });
                    let hover_bg = positive_alpha(if light { 0.20 } else { 0.22 });
                    row.bg(bg)
                        .text_color(positive())
                        .font_weight(FontWeight::SEMIBOLD)
                        .hover(move |s| s.bg(hover_bg))
                }
                TriState::Excluded => {
                    let bg = negative_alpha(if light { 0.14 } else { 0.15 });
                    let hover_bg = negative_alpha(if light { 0.20 } else { 0.22 });
                    row.bg(bg)
                        .text_color(negative())
                        .font_weight(FontWeight::SEMIBOLD)
                        .hover(move |s| s.bg(hover_bg))
                }
            };
            row = row.child(label_area).child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(4.))
                    .child(inc_btn)
                    .child(exc_btn),
            );
            vec![row.into_any_element()]
        }
    }
}

impl RenderOnce for Dropdown {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let light = theme::is_light();

        let mut trigger = div()
            .id(self.id.clone())
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.))
            .h(px(TRIGGER_HEIGHT))
            .px(px(11.))
            .rounded(px(8.))
            .border_1()
            .text_size(px(theme::SMALL))
            .font_weight(FontWeight::MEDIUM)
            .whitespace_nowrap()
            .cursor_pointer();

        if let Some(summary) = self.summary {
            let label_color = if self.open {
                theme::text()
            } else {
                theme::text_dim()
            };
            trigger = trigger
                .child(div().text_color(label_color).child(self.label))
                .child(div().text_color(theme::text_muted()).child("\u{00b7}"))
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::text())
                        .child(summary),
                );
        } else {
            trigger = trigger.child(self.label);
        }

        trigger = trigger.child(
            icon(if self.open {
                "chevron-up"
            } else {
                "chevron-down"
            })
            .size(px(14.))
            .text_color(if self.open || self.active {
                theme::text()
            } else {
                theme::text_muted()
            }),
        );

        trigger = if self.open {
            trigger
                .bg(theme::control_hover())
                .border_color(theme::accent())
        } else if self.active {
            let bg = theme::accent_alpha(if light { 0.14 } else { 0.16 });
            let hover_bg = theme::accent_alpha(if light { 0.20 } else { 0.22 });
            trigger
                .bg(bg)
                .border_color(theme::accent())
                .hover(move |s| s.bg(hover_bg))
        } else {
            trigger
                .bg(theme::control())
                .border_color(theme::control_border())
                .when(light, |t| t.shadow(theme::shadow_sm()))
                .hover(|s| s.bg(theme::control_hover()).text_color(theme::text()))
        };

        // Only attach the open handler while closed: when already open, pressing
        // the trigger fires the menu's `on_mouse_down_out` and closes cleanly
        // without immediately reopening.
        if !self.open
            && let Some(on_open) = self.on_open
        {
            trigger = trigger.on_mouse_down(MouseButton::Left, on_open);
        }

        let menu = self.open.then(|| {
            let on_close = self.on_close;
            let rows: Vec<AnyElement> = self
                .items
                .into_iter()
                .enumerate()
                .flat_map(|(i, item)| render_item(i, item, on_close.clone(), light))
                .collect();

            let mut panel = div()
                .id(child_id(&self.id, "menu"))
                .w(px(self.menu_width))
                .p(px(6.))
                .rounded(px(12.))
                .bg(theme::control())
                .border_1()
                .border_color(theme::control_border())
                .when(light, |p| p.shadow(theme::shadow_lg()))
                .when(!light, |p| p.shadow_lg())
                .occlude()
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation());

            if let Some(handle) = self.focus_handle {
                panel = panel.track_focus(&handle);
            }
            if let Some(close) = on_close.clone() {
                panel = panel.on_key_down(move |ev: &KeyDownEvent, window, cx| {
                    if ev.keystroke.key == "escape" {
                        cx.stop_propagation();
                        close(window, cx);
                    }
                });
            }
            if let Some(close) = on_close {
                panel = panel.on_mouse_down_out(move |_, window, cx| close(window, cx));
            }

            panel = panel.child(
                div()
                    .id(child_id(&self.id, "list"))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .max_h(px(MENU_MAX_HEIGHT))
                    .overflow_y_scroll()
                    .children(rows),
            );

            deferred(
                anchored()
                    .anchor(Corner::TopLeft)
                    .offset(point(px(0.), px(TRIGGER_HEIGHT + 6.)))
                    .snap_to_window_with_margin(Edges::all(px(12.)))
                    .child(panel),
            )
            .with_priority(1)
        });

        div().relative().child(trigger).children(menu)
    }
}
