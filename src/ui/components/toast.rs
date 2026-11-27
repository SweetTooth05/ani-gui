use std::rc::Rc;
use std::time::Duration;

use gpui::{
    Animation, AnimationExt, App, Context, Entity, FontWeight, Global, IntoElement, SharedString,
    Window, div, ease_out_quint, prelude::*, px, quadratic,
};

use crate::ui::components::icon::icon;
use crate::ui::theme;

const SHOW_FOR: Duration = Duration::from_secs(6);
const SHOW_WITH_ACTION: Duration = Duration::from_secs(10);
const ENTER_DURATION: Duration = Duration::from_millis(260);
const EXIT_DURATION: Duration = Duration::from_millis(220);
const ENTER_SLIDE_PX: f32 = 16.;
const EXIT_SLIDE_PX: f32 = 24.;
const MAX_SHOWN: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Info,
    Success,
    Error,
}

type ActionCallback = Rc<dyn Fn(&mut Window, &mut App)>;

/// An optional button rendered inside a toast (e.g. "Restart", "Open folder").
#[derive(Clone)]
pub struct ToastAction {
    pub label: SharedString,
    on_click: ActionCallback,
}

impl ToastAction {
    pub fn new(
        label: impl Into<SharedString>,
        on_click: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            label: label.into(),
            on_click: Rc::new(on_click),
        }
    }
}

/// Specification for a toast notification. Use [`Toast::info`],
/// [`Toast::success`], or [`Toast::error`], optionally attach a coalescing
/// `.key(..)` or `.action(..)`, and finish with `.show(cx)`.
pub struct Toast {
    kind: Kind,
    message: SharedString,
    key: Option<SharedString>,
    action: Option<ToastAction>,
    duration: Option<Duration>,
}

impl Toast {
    pub fn info(message: impl Into<SharedString>) -> Self {
        Self::new(Kind::Info, message)
    }

    pub fn success(message: impl Into<SharedString>) -> Self {
        Self::new(Kind::Success, message)
    }

    pub fn error(message: impl Into<SharedString>) -> Self {
        Self::new(Kind::Error, message)
    }

    fn new(kind: Kind, message: impl Into<SharedString>) -> Self {
        Self {
            kind,
            message: message.into(),
            key: None,
            action: None,
            duration: None,
        }
    }

    /// Coalesces subsequent toasts with the same key into this one in place
    /// instead of stacking duplicates.
    pub fn key(mut self, key: impl Into<SharedString>) -> Self {
        self.key = Some(key.into());
        self
    }

    pub fn action(
        mut self,
        label: impl Into<SharedString>,
        on_click: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        self.action = Some(ToastAction::new(label, on_click));
        self
    }

    pub fn show(self, cx: &mut App) {
        show(cx, self);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Entering,
    Exiting,
}

struct ToastItem {
    id: u64,
    /// Bumped on every coalesced update and on exit so stale timers no-op.
    generation: u64,
    key: Option<SharedString>,
    kind: Kind,
    message: SharedString,
    action: Option<ToastAction>,
    phase: Phase,
}

/// Pure result of adding/coalescing a toast, telling the host which timers to
/// schedule.
struct PushOutcome {
    id: u64,
    generation: u64,
    duration: Duration,
    /// Active toasts transitioned to `Phase::Exiting` because of `MAX_SHOWN`.
    evicted: Vec<(u64, u64)>,
}

/// Short-lived animated messages in the window's bottom-right corner.
#[derive(Default)]
pub struct Toasts {
    items: Vec<ToastItem>,
    next_id: u64,
}

/// The window's `Toasts`, for code without a handle to `Root`.
pub struct ToastHost(pub Entity<Toasts>);

impl Global for ToastHost {}

pub fn info(cx: &mut App, message: impl Into<SharedString>) {
    show(cx, Toast::info(message));
}

pub fn error(cx: &mut App, message: impl Into<SharedString>) {
    show(cx, Toast::error(message));
}

pub fn show(cx: &mut App, toast: Toast) {
    let Some(host) = cx.try_global::<ToastHost>().map(|h| h.0.clone()) else {
        eprintln!("ani-gui: {}", toast.message);
        return;
    };
    host.update(cx, |toasts, cx| toasts.add(toast, cx));
}

impl Toasts {
    fn push_item(&mut self, toast: Toast) -> PushOutcome {
        let duration = toast.duration.unwrap_or_else(|| {
            if toast.action.is_some() {
                SHOW_WITH_ACTION
            } else {
                SHOW_FOR
            }
        });

        if let Some(key) = toast.key {
            // Coalesce into an active toast with the same key, or adopt an
            // active unkeyed toast showing the exact same message.
            let target_idx = self
                .items
                .iter()
                .position(|i| i.phase == Phase::Entering && i.key.as_ref() == Some(&key))
                .or_else(|| {
                    self.items.iter().position(|i| {
                        i.phase == Phase::Entering && i.key.is_none() && i.message == toast.message
                    })
                });
            if let Some(idx) = target_idx {
                let item = &mut self.items[idx];
                item.key = Some(key);
                item.kind = toast.kind;
                item.message = toast.message;
                item.action = toast.action;
                item.generation += 1;
                return PushOutcome {
                    id: item.id,
                    generation: item.generation,
                    duration,
                    evicted: Vec::new(),
                };
            }
            // Drop any fading-out toast for the same key so the new one enters cleanly.
            self.items
                .retain(|i| !(i.phase == Phase::Exiting && i.key.as_ref() == Some(&key)));
            return self.insert_fresh(Some(key), toast.kind, toast.message, toast.action, duration);
        }

        // Unkeyed duplicate: coalesce instead of stacking identical text.
        if toast.action.is_none()
            && let Some(item) = self.items.iter_mut().find(|i| {
                i.phase == Phase::Entering
                    && i.key.is_none()
                    && i.kind == toast.kind
                    && i.message == toast.message
                    && i.action.is_none()
            })
        {
            item.generation += 1;
            return PushOutcome {
                id: item.id,
                generation: item.generation,
                duration,
                evicted: Vec::new(),
            };
        }

        self.insert_fresh(None, toast.kind, toast.message, toast.action, duration)
    }

    fn insert_fresh(
        &mut self,
        key: Option<SharedString>,
        kind: Kind,
        message: SharedString,
        action: Option<ToastAction>,
        duration: Duration,
    ) -> PushOutcome {
        let id = self.next_id;
        self.next_id += 1;
        self.items.push(ToastItem {
            id,
            generation: 0,
            key,
            kind,
            message,
            action,
            phase: Phase::Entering,
        });
        let mut evicted = Vec::new();
        let active_count = self
            .items
            .iter()
            .filter(|i| i.phase == Phase::Entering)
            .count();
        if active_count > MAX_SHOWN {
            let to_evict = active_count - MAX_SHOWN;
            let mut count = 0;
            for item in &mut self.items {
                if count >= to_evict {
                    break;
                }
                if item.phase == Phase::Entering {
                    item.phase = Phase::Exiting;
                    item.generation += 1;
                    evicted.push((item.id, item.generation));
                    count += 1;
                }
            }
        }
        PushOutcome {
            id,
            generation: 0,
            duration,
            evicted,
        }
    }

    /// Transitions toast `id` to `Phase::Exiting`. When `expected_gen` is
    /// `Some`, only exits if the toast has not been updated since.
    fn begin_exit(&mut self, id: u64, expected_gen: Option<u64>) -> Option<u64> {
        let item = self.items.iter_mut().find(|i| i.id == id)?;
        if item.phase == Phase::Exiting {
            return None;
        }
        if expected_gen.is_some_and(|g| item.generation != g) {
            return None;
        }
        item.phase = Phase::Exiting;
        item.generation += 1;
        Some(item.generation)
    }

    /// Removes toast `id` once its exit animation finishes.
    fn finish_exit(&mut self, id: u64, exit_gen: u64) -> bool {
        let before = self.items.len();
        self.items
            .retain(|i| !(i.id == id && i.phase == Phase::Exiting && i.generation == exit_gen));
        self.items.len() != before
    }

    fn add(&mut self, toast: Toast, cx: &mut Context<Self>) {
        let outcome = self.push_item(toast);
        cx.notify();
        for (evicted_id, evicted_gen) in outcome.evicted {
            self.schedule_removal(evicted_id, evicted_gen, cx);
        }
        let id = outcome.id;
        let generation = outcome.generation;
        let duration = outcome.duration;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(duration).await;
            this.update(cx, |toasts, cx| {
                if let Some(exit_gen) = toasts.begin_exit(id, Some(generation)) {
                    cx.notify();
                    toasts.schedule_removal(id, exit_gen, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn dismiss(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(exit_gen) = self.begin_exit(id, None) {
            cx.notify();
            self.schedule_removal(id, exit_gen, cx);
        }
    }

    fn schedule_removal(&self, id: u64, exit_gen: u64, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(EXIT_DURATION).await;
            this.update(cx, |toasts, cx| {
                if toasts.finish_exit(id, exit_gen) {
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }
}

impl Render for Toasts {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .absolute()
            .bottom(px(24.))
            .right(px(24.))
            // Stay inside the main content area so toasts never cover the sidebar control.
            .left(px(theme::SIDEBAR_WIDTH + 16.))
            .flex()
            .flex_col()
            .items_end()
            .gap_2()
            .children(self.items.iter().map(|item| {
                let id = item.id;
                let kind = item.kind;
                let phase = item.phase;
                let (icon_name, accent_color) = match kind {
                    Kind::Info => ("info", theme::text_dim()),
                    Kind::Success => ("check", theme::accent()),
                    Kind::Error => ("circle-alert", theme::danger()),
                };
                let light = theme::is_light();
                let action_el = item.action.clone().map(|action| {
                    let on_click = action.on_click.clone();
                    let accent_btn = kind == Kind::Success;
                    div()
                        .id(("toast-action", id))
                        .flex_none()
                        .px(px(10.))
                        .py(px(5.))
                        .rounded(px(6.))
                        .text_size(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .whitespace_nowrap()
                        .cursor_pointer()
                        .bg(if accent_btn {
                            theme::accent()
                        } else if light {
                            theme::night()
                        } else {
                            theme::raised_hover()
                        })
                        .when(light && !accent_btn, |d| {
                            d.border_1().border_color(theme::control_border())
                        })
                        .text_color(if accent_btn {
                            theme::on_accent()
                        } else {
                            theme::text()
                        })
                        .hover(move |s| {
                            s.bg(if accent_btn {
                                theme::accent_hover()
                            } else if light {
                                theme::ghost_hover()
                            } else {
                                theme::line()
                            })
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            (on_click)(window, cx);
                            this.dismiss(id, cx);
                        }))
                        .child(action.label)
                });
                let card = div()
                    .id(("toast", id))
                    .max_w(px(420.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(14.))
                    .py(px(11.))
                    .rounded(px(10.))
                    .bg(theme::control())
                    .border_l(px(3.))
                    .border_color(accent_color)
                    .when(!light, |d| d.shadow_lg())
                    .when(light, |d| d.shadow(theme::shadow_lg()))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.dismiss(id, cx)))
                    .child(icon(icon_name).size(px(16.)).text_color(accent_color))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(theme::SMALL))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::text())
                            .child(item.message.clone()),
                    )
                    .children(action_el)
                    .child(icon("x").size(px(14.)).text_color(theme::text_muted()));
                match phase {
                    Phase::Entering => card
                        .with_animation(
                            ("toast-enter", id),
                            Animation::new(ENTER_DURATION).with_easing(ease_out_quint()),
                            |el, delta| {
                                el.relative()
                                    .top(px(ENTER_SLIDE_PX * (1.0 - delta)))
                                    .opacity(delta)
                            },
                        )
                        .into_any_element(),
                    Phase::Exiting => card
                        .with_animation(
                            ("toast-exit", id),
                            Animation::new(EXIT_DURATION).with_easing(quadratic),
                            |el, delta| {
                                el.relative()
                                    .left(px(EXIT_SLIDE_PX * delta))
                                    .opacity((1.0 - delta).max(0.0))
                            },
                        )
                        .into_any_element(),
                }
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyed_progress_coalesces_into_one_toast_and_bumps_generation() {
        let mut toasts = Toasts::default();

        let first = toasts.push_item(Toast::info("Downloading ep 1 (10%)").key("dl:1"));
        assert_eq!(toasts.items.len(), 1);
        assert_eq!(first.generation, 0);

        let second = toasts.push_item(Toast::info("Downloading ep 1 (50%)").key("dl:1"));
        assert_eq!(toasts.items.len(), 1);
        assert_eq!(second.id, first.id);
        assert_eq!(second.generation, 1);
        assert_eq!(toasts.items[0].message.as_ref(), "Downloading ep 1 (50%)");

        let done = toasts.push_item(Toast::success("Downloaded ep 1").key("dl:1"));
        assert_eq!(toasts.items.len(), 1);
        assert_eq!(done.id, first.id);
        assert_eq!(done.generation, 2);
        assert_eq!(toasts.items[0].kind, Kind::Success);
        assert_eq!(toasts.items[0].message.as_ref(), "Downloaded ep 1");

        // Stale timer from the first generation must not dismiss the updated toast.
        assert_eq!(toasts.begin_exit(first.id, Some(first.generation)), None);
        assert_eq!(toasts.items[0].phase, Phase::Entering);

        // Current generation triggers exit, then finish_exit removes it.
        let exit_gen = toasts
            .begin_exit(done.id, Some(done.generation))
            .expect("current generation exits");
        assert_eq!(toasts.items[0].phase, Phase::Exiting);
        assert!(toasts.finish_exit(done.id, exit_gen));
        assert!(toasts.items.is_empty());
    }

    #[test]
    fn keyed_toast_adopts_matching_unkeyed_toast() {
        let mut toasts = Toasts::default();

        let unkeyed = toasts.push_item(Toast::info("Downloading Frieren episode 1"));
        let keyed =
            toasts.push_item(Toast::info("Downloading Frieren episode 1").key("download:abc"));

        assert_eq!(toasts.items.len(), 1);
        assert_eq!(keyed.id, unkeyed.id);
        assert_eq!(
            toasts.items[0].key.as_ref().map(|k| k.as_ref()),
            Some("download:abc")
        );
    }

    #[test]
    fn exiting_keyed_toast_is_replaced_cleanly_by_new_event() {
        let mut toasts = Toasts::default();

        let first = toasts.push_item(Toast::info("Downloading ep 1").key("dl:1"));
        let exit_gen = toasts.begin_exit(first.id, None).unwrap();
        assert_eq!(toasts.items[0].phase, Phase::Exiting);

        // A new toast for the same key while exiting spawns a fresh entering toast.
        let second = toasts.push_item(Toast::success("Downloaded ep 1").key("dl:1"));
        assert_eq!(toasts.items.len(), 1);
        assert_ne!(second.id, first.id);
        assert_eq!(toasts.items[0].phase, Phase::Entering);
        // Old exit completion is a no-op.
        assert!(!toasts.finish_exit(first.id, exit_gen));
        assert_eq!(toasts.items.len(), 1);
    }

    #[test]
    fn distinct_keys_stack_and_evict_oldest_with_exit_animation() {
        let mut toasts = Toasts::default();
        for i in 0..MAX_SHOWN {
            let out = toasts.push_item(Toast::info(format!("Event {i}")).key(format!("k:{i}")));
            assert!(out.evicted.is_empty());
        }
        assert_eq!(toasts.items.len(), MAX_SHOWN);

        let fifth = toasts.push_item(Toast::info("Event 4").key("k:4"));
        assert_eq!(fifth.evicted.len(), 1);
        let (evicted_id, evicted_gen) = fifth.evicted[0];
        assert_eq!(evicted_id, 0);
        assert_eq!(toasts.items[0].phase, Phase::Exiting);

        let active = toasts
            .items
            .iter()
            .filter(|i| i.phase == Phase::Entering)
            .count();
        assert_eq!(active, MAX_SHOWN);

        assert!(toasts.finish_exit(evicted_id, evicted_gen));
        assert_eq!(toasts.items.len(), MAX_SHOWN);
    }

    #[test]
    fn duplicate_unkeyed_messages_coalesce() {
        let mut toasts = Toasts::default();
        let a = toasts.push_item(Toast::error("Network error"));
        let b = toasts.push_item(Toast::error("Network error"));
        assert_eq!(toasts.items.len(), 1);
        assert_eq!(a.id, b.id);
        assert_eq!(b.generation, 1);

        // Different kind does not coalesce.
        let c = toasts.push_item(Toast::info("Network error"));
        assert_eq!(toasts.items.len(), 2);
        assert_ne!(c.id, a.id);
    }

    #[test]
    fn dismiss_is_idempotent_while_exiting() {
        let mut toasts = Toasts::default();
        let a = toasts.push_item(Toast::success("Done"));
        let exit_gen = toasts.begin_exit(a.id, None);
        assert!(exit_gen.is_some());
        assert_eq!(toasts.begin_exit(a.id, None), None);
    }

    #[test]
    fn action_toasts_get_longer_display_duration() {
        let mut toasts = Toasts::default();
        let plain = toasts.push_item(Toast::info("Plain"));
        let with_action = toasts.push_item(
            Toast::success("ani-gui v0.0.2 installed — restart to use it")
                .key("updater")
                .action("Restart", |_, _| {}),
        );
        assert_eq!(plain.duration, SHOW_FOR);
        assert_eq!(with_action.duration, SHOW_WITH_ACTION);
        assert!(with_action.duration > plain.duration);
    }
}
