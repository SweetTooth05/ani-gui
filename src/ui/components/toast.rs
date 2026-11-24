use std::time::Duration;

use gpui::{
    App, Context, Entity, FontWeight, Global, IntoElement, SharedString, Window, div, prelude::*,
    px,
};

use crate::ui::theme;

const SHOW_FOR: Duration = Duration::from_secs(6);
const MAX_SHOWN: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Info,
    Error,
}

/// Short-lived messages in the window's bottom-right corner.
#[derive(Default)]
pub struct Toasts {
    items: Vec<(u64, Kind, SharedString)>,
    next_id: u64,
}

/// The window's `Toasts`, for code without a handle to `Root`.
pub struct ToastHost(pub Entity<Toasts>);

impl Global for ToastHost {}

pub fn info(cx: &mut App, message: impl Into<SharedString>) {
    push(cx, Kind::Info, message.into());
}

pub fn error(cx: &mut App, message: impl Into<SharedString>) {
    push(cx, Kind::Error, message.into());
}

fn push(cx: &mut App, kind: Kind, message: SharedString) {
    let Some(host) = cx.try_global::<ToastHost>().map(|h| h.0.clone()) else {
        eprintln!("ani-gui: {message}");
        return;
    };
    host.update(cx, |toasts, cx| toasts.add(kind, message, cx));
}

impl Toasts {
    fn add(&mut self, kind: Kind, message: SharedString, cx: &mut Context<Self>) {
        let id = self.next_id;
        self.next_id += 1;
        self.items.push((id, kind, message));
        if self.items.len() > MAX_SHOWN {
            self.items.remove(0);
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SHOW_FOR).await;
            this.update(cx, |toasts, cx| {
                toasts.items.retain(|(i, _, _)| *i != id);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn dismiss(&mut self, id: u64, cx: &mut Context<Self>) {
        self.items.retain(|(i, _, _)| *i != id);
        cx.notify();
    }
}

impl Render for Toasts {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .absolute()
            .bottom(px(24.))
            .right(px(24.))
            .flex()
            .flex_col()
            .items_end()
            .gap_2()
            .children(self.items.iter().map(|(id, kind, message)| {
                let id = *id;
                div()
                    .id(("toast", id))
                    .max_w(px(420.))
                    .px_4()
                    .py_3()
                    .rounded(px(10.))
                    .bg(theme::raised())
                    .border_l(px(3.))
                    .border_color(match kind {
                        Kind::Info => theme::accent(),
                        Kind::Error => theme::danger(),
                    })
                    .shadow_lg()
                    .text_size(px(theme::SMALL))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme::text())
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.dismiss(id, cx)))
                    .child(message.clone())
            }))
    }
}
