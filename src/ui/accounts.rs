//! MyAnimeList / AniList accounts in the UI: starting `crate::lists` at
//! launch, telling pages when list data changes, and the Accounts section
//! of Settings.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::anyhow;
use futures::StreamExt;
use futures::channel::oneshot;
use gpui::{
    AnyElement, App, ClipboardItem, Context, Entity, FontWeight, Global, IntoElement, SharedString,
    Subscription, Task, Window, div, prelude::*, px, relative,
};

use crate::app::AppState;
use crate::db;
use crate::lists::{self, Account, Event, Service};
use crate::ui::components::button::{ButtonKind, button};
use crate::ui::components::icon::icon;
use crate::ui::components::text_input::{Changed, TextInput};
use crate::ui::components::toast;
use crate::ui::theme;

/// Notified (on the UI thread) whenever accounts or imported lists change.
pub struct ListsChanged;

struct ListsHost(Entity<ListsChanged>);

impl Global for ListsHost {}

/// Starts account syncing once the library is open: relays background
/// events to pages and toasts, and syncs accounts not synced lately.
pub fn start(cx: &mut App) {
    let Some(db) = cx.try_global::<AppState>().map(|s| s.db.clone()) else {
        return;
    };
    let Some(mut events) = lists::init(db) else {
        return;
    };
    let changed = cx.new(|_| ListsChanged);
    cx.set_global(ListsHost(changed.clone()));
    cx.spawn(async move |cx| {
        while let Some(first) = events.next().await {
            let mut batch = vec![first];
            while let Ok(event) = events.try_recv() {
                batch.push(event);
            }
            let alive = cx.update(|cx| {
                for event in batch {
                    match event {
                        Event::Finished { title } => {
                            toast::info(cx, format!("You finished {title}. Rate it on its page."))
                        }
                        Event::AuthLost(service) => toast::error(
                            cx,
                            format!("Your {service} sign-in expired. Connect again in Settings."),
                        ),
                        Event::Changed => {}
                    }
                }
                changed.update(cx, |_, cx| cx.notify());
            });
            if alive.is_err() {
                break;
            }
        }
    })
    .detach();
    lists::sync_all_background(false);
}

/// Calls `f` whenever list data changes (None before `start`).
pub fn observe<V: 'static>(
    cx: &mut Context<V>,
    mut f: impl FnMut(&mut V, &mut Context<V>) + 'static,
) -> Option<Subscription> {
    let host = cx.try_global::<ListsHost>()?.0.clone();
    Some(cx.observe(&host, move |view, _, cx| f(view, cx)))
}

/// "just now", "5 minutes ago", "3 hours ago", "2 days ago".
pub fn ago(then: i64, now: i64) -> String {
    let secs = (now - then).max(0);
    let plural = |n: i64, unit: &str| {
        if n == 1 {
            format!("1 {unit} ago")
        } else {
            format!("{n} {unit}s ago")
        }
    };
    match secs {
        0..60 => "just now".into(),
        60..3600 => plural(secs / 60, "minute"),
        3600..86_400 => plural(secs / 3600, "hour"),
        _ => plural(secs / 86_400, "day"),
    }
}

/// What a service needs explained in Settings.
struct ServiceInfo {
    service: Service,
    blurb: &'static str,
    developer_page: &'static str,
    redirect_uri: &'static str,
    steps: &'static str,
}

const MAL: ServiceInfo = ServiceInfo {
    service: Service::Mal,
    blurb: "Imports your anime list, marks episodes you finish and sends your ratings.",
    developer_page: lists::MAL_DEVELOPER_PAGE,
    redirect_uri: lists::MAL_REDIRECT_URI,
    steps: "Create an API app (App Type: other) with this App Redirect URL, then paste its Client ID.",
};

const ANILIST: ServiceInfo = ServiceInfo {
    service: Service::AniList,
    blurb: "Imports your anime list, marks episodes you finish and sends your ratings.",
    developer_page: lists::ANILIST_DEVELOPER_PAGE,
    redirect_uri: lists::ANILIST_REDIRECT_URI,
    steps: "Create a client with this Redirect URL, then paste its ID. AniList shows a token to paste back here.",
};

pub struct AccountsSection {
    accounts: Vec<Account>,
    client_ids: HashMap<Service, Entity<TextInput>>,
    /// Services with a built-in client ID where the user opened the custom-ID override.
    custom_open: HashSet<Service>,
    token: Entity<TextInput>,
    /// AniList's token page was opened; the paste field shows.
    awaiting_token: bool,
    /// Services being connected or disconnected.
    working: HashSet<Service>,
    /// Stops a MAL sign-in waiting for the browser.
    mal_cancel: Option<Arc<AtomicBool>>,
    /// Why the last connect failed, per service.
    errors: HashMap<Service, String>,
    /// Client IDs are filled from the database once, not over typing.
    filled: bool,
    load_task: Option<Task<()>>,
    _changed: Option<Subscription>,
    /// Re-render as client IDs are typed (Connect enables).
    _typing: Vec<Subscription>,
}

impl AccountsSection {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let client_ids: HashMap<Service, Entity<TextInput>> = Service::ALL
            .into_iter()
            .map(|s| (s, cx.new(|cx| TextInput::new("Client ID", cx))))
            .collect();
        let typing = client_ids
            .values()
            .map(|input| cx.subscribe(input, |_, _, _: &Changed, cx| cx.notify()))
            .collect();
        let changed = observe(cx, |section: &mut Self, cx| section.reload(cx));
        let mut section = AccountsSection {
            accounts: Service::ALL.into_iter().map(Account::empty).collect(),
            client_ids,
            custom_open: HashSet::new(),
            token: cx.new(|cx| TextInput::new("Paste the token here", cx)),
            awaiting_token: false,
            working: HashSet::new(),
            mal_cancel: None,
            errors: HashMap::new(),
            filled: false,
            load_task: None,
            _changed: changed,
            _typing: typing,
        };
        section.reload(cx);
        section
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(db) = cx.try_global::<AppState>().map(|s| s.db.clone()) else {
            return;
        };
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let accounts = cx
                .background_spawn(async move { lists::accounts(&db) })
                .await;
            this.update(cx, |section, cx| {
                if !section.filled {
                    section.filled = true;
                    for account in &accounts {
                        let builtin = lists::builtin_client_id(account.service);
                        let is_custom = !account.client_id.is_empty()
                            && builtin.is_none_or(|b| b != account.client_id);
                        if is_custom {
                            if let Some(input) = section.client_ids.get(&account.service) {
                                let id = account.client_id.clone();
                                input.update(cx, |input, cx| input.set_text(id, cx));
                            }
                            if builtin.is_some() {
                                section.custom_open.insert(account.service);
                            }
                        }
                    }
                }
                section.accounts = accounts;
                cx.notify();
            })
            .ok();
        }));
    }

    fn account(&self, service: Service) -> Account {
        self.accounts
            .iter()
            .find(|a| a.service == service)
            .cloned()
            .unwrap_or_else(|| Account::empty(service))
    }

    fn client_id(&self, service: Service, cx: &Context<Self>) -> String {
        self.client_ids
            .get(&service)
            .map(|input| input.read(cx).text().trim().to_string())
            .unwrap_or_default()
    }

    fn effective_client_id(&self, service: Service, cx: &Context<Self>) -> String {
        let builtin = lists::builtin_client_id(service);
        if builtin.is_some() && !self.custom_open.contains(&service) {
            builtin.unwrap_or_default().to_string()
        } else {
            lists::effective_client_id(&self.client_id(service, cx), builtin).to_string()
        }
    }

    fn toggle_custom(&mut self, service: Service, cx: &mut Context<Self>) {
        if self.custom_open.remove(&service) {
            if let Some(input) = self.client_ids.get(&service) {
                input.update(cx, |input, cx| input.set_text("", cx));
            }
        } else {
            self.custom_open.insert(service);
        }
        self.errors.remove(&service);
        cx.notify();
    }

    /// Runs blocking `work` on a thread of its own (a sign-in can wait on
    /// the browser for minutes), then `done` on the UI thread.
    fn run<T: Send + 'static>(
        &mut self,
        service: Service,
        work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
        done: impl FnOnce(&mut Self, T, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        self.working.insert(service);
        self.errors.remove(&service);
        cx.notify();
        let (tx, rx) = oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name(format!("{}-account", service.as_str()))
            .spawn(move || {
                let _ = tx.send(work());
            });
        if let Err(err) = spawned {
            self.working.remove(&service);
            self.errors
                .insert(service, format!("Couldn't start: {err}"));
            return;
        }
        cx.spawn(async move |this, cx| {
            let result = rx
                .await
                .unwrap_or_else(|_| Err(anyhow!("stopped unexpectedly")));
            this.update(cx, |section, cx| {
                section.working.remove(&service);
                match result {
                    Ok(value) => done(section, value, cx),
                    Err(err) => {
                        section.errors.insert(service, format!("{err:#}"));
                    }
                }
                section.reload(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn connect(&mut self, service: Service, cx: &mut Context<Self>) {
        let client_id = self.effective_client_id(service, cx);
        match service {
            Service::Mal => {
                let cancel = Arc::new(AtomicBool::new(false));
                self.mal_cancel = Some(cancel.clone());
                self.run(
                    service,
                    move || lists::connect_mal(&client_id, cancel),
                    |section, name, cx| {
                        section.mal_cancel = None;
                        toast::info(cx, format!("Connected to MyAnimeList as {name}"));
                    },
                    cx,
                );
            }
            Service::AniList => self.run(
                service,
                move || {
                    let url = lists::anilist_authorize_url(&client_id)?;
                    lists::set_client_id(service, &client_id)?;
                    lists::open_in_browser(&url)
                },
                |section, (), cx| {
                    section.awaiting_token = true;
                    cx.notify();
                },
                cx,
            ),
        }
    }

    fn cancel(&mut self, service: Service, cx: &mut Context<Self>) {
        match service {
            Service::Mal => {
                if let Some(cancel) = self.mal_cancel.take() {
                    cancel.store(true, Ordering::SeqCst);
                }
            }
            Service::AniList => {
                self.awaiting_token = false;
                self.token.update(cx, |input, cx| input.set_text("", cx));
            }
        }
        cx.notify();
    }

    fn save_token(&mut self, cx: &mut Context<Self>) {
        let client_id = self.effective_client_id(Service::AniList, cx);
        let token = self.token.read(cx).text().to_string();
        // The token leaves the field (and the screen) straight away.
        self.token.update(cx, |input, cx| input.set_text("", cx));
        self.run(
            Service::AniList,
            move || lists::connect_anilist(&client_id, &token),
            |section, name, cx| {
                section.awaiting_token = false;
                toast::info(cx, format!("Connected to AniList as {name}"));
            },
            cx,
        );
    }

    fn disconnect(&mut self, service: Service, cx: &mut Context<Self>) {
        self.run(
            service,
            move || lists::disconnect(service),
            move |_, (), cx| toast::info(cx, format!("Disconnected {service}")),
            cx,
        );
    }

    fn sync(&mut self, service: Service, cx: &mut Context<Self>) {
        cx.background_spawn(async move {
            if let Err(err) = lists::sync_now(service) {
                eprintln!("ani-gui: syncing {service}: {err:#}");
            }
        })
        .detach();
        cx.notify();
    }

    fn render_card(&self, info: &ServiceInfo, cx: &mut Context<Self>) -> AnyElement {
        let service = info.service;
        let account = self.account(service);
        let working = self.working.contains(&service);
        let syncing = lists::is_syncing(service);
        let id = service.as_str();
        let status = match (&account.user_name, account.connected()) {
            (Some(name), true) => status_chip(format!("Connected as {name}"), true),
            _ => status_chip("Not connected".into(), false),
        };
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(16.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(px(17.))
                            .child(service.label()),
                    )
                    .child(
                        div()
                            .text_size(px(theme::SMALL))
                            .text_color(theme::text_muted())
                            .child(info.blurb),
                    ),
            )
            .child(status);

        let body = if account.connected() {
            let synced = match account.last_sync {
                _ if syncing => "Syncing…".to_string(),
                Some(at) => format!("Last synced {}", ago(at, db::now())),
                None => "Not synced yet".to_string(),
            };
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(16.))
                .child(
                    div()
                        .text_size(px(theme::SMALL))
                        .text_color(theme::text_dim())
                        .child(synced),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(
                            button(SharedString::from(format!("{id}-sync")), "Sync now")
                                .kind(ButtonKind::Secondary)
                                .compact()
                                .disabled(syncing || working)
                                .on_click(cx.listener(move |s, _, _, cx| s.sync(service, cx))),
                        )
                        .child(
                            button(SharedString::from(format!("{id}-disconnect")), "Disconnect")
                                .kind(ButtonKind::Ghost)
                                .compact()
                                .disabled(working)
                                .on_click(
                                    cx.listener(move |s, _, _, cx| s.disconnect(service, cx)),
                                ),
                        ),
                )
                .into_any_element()
        } else {
            self.render_setup(info, working, cx)
        };

        let error = self
            .errors
            .get(&service)
            .cloned()
            .or(account.last_error.clone());
        div()
            .flex()
            .flex_col()
            .gap(px(16.))
            .p(px(20.))
            .rounded(px(12.))
            .bg(theme::deep())
            .border_1()
            .border_color(theme::line())
            .when(theme::is_light(), |d| d.shadow(theme::shadow_sm()))
            .child(header)
            .child(body)
            .children(error.map(|err| {
                div()
                    .flex()
                    .items_start()
                    .gap(px(8.))
                    .text_size(px(theme::SMALL))
                    .line_height(relative(1.45))
                    .text_color(theme::danger())
                    .child(
                        icon("circle-alert")
                            .size(px(16.))
                            .mt(px(1.))
                            .text_color(theme::danger()),
                    )
                    .child(div().flex_1().min_w_0().child(err))
            }))
            .into_any_element()
    }

    /// Connect button (one-click when a built-in client ID is present, or
    /// custom Client ID + redirect URI setup) for an account not connected.
    fn render_setup(
        &self,
        info: &ServiceInfo,
        working: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let service = info.service;
        let id = service.as_str();
        let uri = info.redirect_uri;
        let page = info.developer_page;
        let has_builtin = lists::builtin_client_id(service).is_some();
        let show_custom = !has_builtin || self.custom_open.contains(&service);
        let has_id = !self.client_id(service, cx).is_empty();
        let waiting_browser = working && service == Service::Mal && self.mal_cancel.is_some();
        let pasting = service == Service::AniList && self.awaiting_token;

        let connect_btn = |disabled: bool| {
            if waiting_browser {
                button(SharedString::from(format!("{id}-cancel")), "Cancel")
                    .kind(ButtonKind::Secondary)
                    .on_click(cx.listener(move |s, _, _, cx| s.cancel(service, cx)))
            } else {
                button(
                    SharedString::from(format!("{id}-connect")),
                    if working { "Connecting…" } else { "Connect" },
                )
                .kind(ButtonKind::Secondary)
                .disabled(disabled)
                .on_click(cx.listener(move |s, _, _, cx| s.connect(service, cx)))
            }
        };

        let mut setup = div().flex().flex_col().gap(px(14.));

        if !show_custom {
            setup = setup.child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(12.))
                    .child(connect_btn(working || pasting))
                    .child(
                        button(
                            SharedString::from(format!("{id}-custom-id")),
                            "Use custom Client ID",
                        )
                        .kind(ButtonKind::Ghost)
                        .compact()
                        .disabled(working || pasting)
                        .on_click(cx.listener(move |s, _, _, cx| s.toggle_custom(service, cx))),
                    ),
            );
        } else {
            let register = div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(
                    div()
                        .text_size(px(theme::SMALL))
                        .line_height(relative(1.45))
                        .text_color(theme::text_dim())
                        .child(info.steps),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .px(px(12.))
                                .py(px(7.))
                                .rounded(px(8.))
                                .bg(if theme::is_light() {
                                    theme::night()
                                } else {
                                    theme::raised()
                                })
                                .when(theme::is_light(), |d| {
                                    d.border_1().border_color(theme::line())
                                })
                                .text_size(px(theme::SMALL))
                                .font_weight(FontWeight::MEDIUM)
                                .child(uri),
                        )
                        .child(
                            button(SharedString::from(format!("{id}-copy-uri")), "Copy")
                                .kind(ButtonKind::Ghost)
                                .compact()
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        uri.to_string(),
                                    ));
                                    toast::info(cx, "Redirect URL copied");
                                }),
                        )
                        .child(
                            button(
                                SharedString::from(format!("{id}-dev-page")),
                                "Open developer page",
                            )
                            .kind(ButtonKind::Ghost)
                            .compact()
                            .on_click(move |_, _, cx| open_page(page, cx)),
                        ),
                );

            let id_row = div()
                .flex()
                .items_center()
                .gap(px(10.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(self.client_ids[&service].clone()),
                )
                .child(connect_btn(working || !has_id || pasting));

            setup = setup.child(register).child(id_row);
            if has_builtin {
                setup = setup.child(
                    div().flex().child(
                        button(
                            SharedString::from(format!("{id}-builtin-id")),
                            "Use built-in Client ID",
                        )
                        .kind(ButtonKind::Ghost)
                        .compact()
                        .disabled(working || pasting)
                        .on_click(cx.listener(move |s, _, _, cx| s.toggle_custom(service, cx))),
                    ),
                );
            }
        }

        if waiting_browser {
            setup = setup.child(hint(
                "Finish signing in in your browser. This waits for up to five minutes.",
            ));
        }
        if pasting {
            setup = setup.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(hint(
                        "Approve ani-gui in the browser, then copy the token AniList shows and paste it here.",
                    ))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(div().flex_1().min_w_0().child(self.token.clone()))
                            .child(
                                button("anilist-save-token", "Save token")
                                    .kind(ButtonKind::Secondary)
                                    .disabled(working)
                                    .on_click(cx.listener(|s, _, _, cx| s.save_token(cx))),
                            )
                            .child(
                                button("anilist-cancel-token", "Cancel")
                                    .kind(ButtonKind::Ghost)
                                    .on_click(
                                        cx.listener(move |s, _, _, cx| s.cancel(service, cx)),
                                    ),
                            ),
                    ),
            );
        }
        setup.into_any_element()
    }
}

impl Render for AccountsSection {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .text_size(px(theme::SMALL))
                    .line_height(relative(1.45))
                    .text_color(theme::text_muted())
                    .child(
                        "Connect either or both. Sign-ins are kept in your system keychain, \
                         never in ani-gui's files.",
                    ),
            )
            .child(self.render_card(&MAL, cx))
            .child(self.render_card(&ANILIST, cx))
    }
}

fn status_chip(label: String, connected: bool) -> impl IntoElement {
    let light = theme::is_light();
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.))
        .px(px(10.))
        .py(px(5.))
        .rounded_full()
        .when(light && connected, |d| {
            d.bg(theme::accent_alpha(0.10))
                .border_1()
                .border_color(theme::accent_alpha(0.22))
        })
        .when(light && !connected, |d| {
            d.bg(theme::night()).border_1().border_color(theme::line())
        })
        .when(!light, |d| d.bg(theme::raised()))
        .text_size(px(theme::SMALL))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(if connected {
            theme::text()
        } else {
            theme::text_muted()
        })
        .child(div().size(px(7.)).rounded_full().bg(if connected {
            theme::accent()
        } else {
            theme::text_muted()
        }))
        .child(label)
}

fn hint(text: &'static str) -> impl IntoElement {
    div()
        .text_size(px(theme::SMALL))
        .line_height(relative(1.45))
        .text_color(theme::text_dim())
        .child(text)
}

fn open_page(url: &'static str, cx: &mut App) {
    let opened = cx.background_spawn(async move { lists::open_in_browser(url) });
    cx.spawn(async move |cx| {
        if let Err(err) = opened.await {
            cx.update(|cx| toast::error(cx, format!("Couldn't open the browser: {err:#}")))
                .ok();
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ago_reads_naturally() {
        assert_eq!(ago(1000, 1000), "just now");
        assert_eq!(ago(1000, 1059), "just now");
        assert_eq!(ago(1000, 1060), "1 minute ago");
        assert_eq!(ago(1000, 1000 + 5 * 60), "5 minutes ago");
        assert_eq!(ago(0, 3600), "1 hour ago");
        assert_eq!(ago(0, 7 * 3600), "7 hours ago");
        assert_eq!(ago(0, 86_400 * 3), "3 days ago");
        // A clock that went backwards isn't "in the future".
        assert_eq!(ago(2000, 1000), "just now");
    }
}
