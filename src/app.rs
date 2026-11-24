use std::env;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use futures::StreamExt;
use gpui::{
    App, Context, Entity, EntityId, FocusHandle, Global, KeyBinding, MouseButton,
    NavigationDirection, SharedString, Window, actions, div, prelude::*, px,
};

use crate::db::Db;
use crate::downloads::DownloadManager;
use crate::model::{Mode, Settings, ShowSummary, Stream};
use crate::ui::calendar::CalendarPage;
use crate::ui::components::toast::{self, Toasts};
use crate::ui::downloads_page::DownloadsPage;
use crate::ui::home::HomePage;
use crate::ui::my_list::MyListPage;
use crate::ui::player_view::{self, PlayerView};
use crate::ui::search::SearchPage;
use crate::ui::settings_page::SettingsPage;
use crate::ui::show::ShowPage;
use crate::ui::{sidebar, theme};
use crate::{anicli, paths};

actions!(ani_gui, [Back]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("alt-left", Back, None)]);
    player_view::bind_keys(cx);
}

/// Shared app services, set once startup has opened the database.
pub struct AppState {
    pub db: Arc<Mutex<Db>>,
    pub settings: Settings,
    pub downloads: Arc<DownloadManager>,
    pub cache_dir: PathBuf,
    /// ani-cli's shim dir (`anicli::ensure_shims`), or why it could not be written.
    pub shims: Result<PathBuf, String>,
    /// Notified (on the UI thread) whenever the download list changes.
    pub downloads_changed: Entity<DownloadsChanged>,
}

impl Global for AppState {}

pub struct DownloadsChanged;

#[derive(Clone, Debug, PartialEq)]
pub enum Route {
    Home,
    Search,
    MyList,
    Calendar,
    Downloads,
    Settings,
    Show(ShowRef),
    Player(PlayRequest),
}

/// A show to open: the AllAnime entry when known, else the AniList id the
/// Show page resolves it from.
#[derive(Clone, Debug, PartialEq)]
pub struct ShowRef {
    pub allanime: Option<ShowSummary>,
    pub anilist_id: Option<i64>,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlayRequest {
    pub show: ShowSummary,
    pub mode: Mode,
    pub ep: String,
    pub start: f64,
    pub episodes: Vec<String>,
    /// Stream quality for this play and the episodes autoplayed after it.
    pub quality: String,
    pub local_file: Option<PathBuf>,
    /// Already resolved by the Show page; None means the player resolves it.
    pub stream: Option<Stream>,
}

enum Page {
    Home(Entity<HomePage>),
    Search(Entity<SearchPage>),
    MyList(Entity<MyListPage>),
    Calendar(Entity<CalendarPage>),
    Downloads(Entity<DownloadsPage>),
    Settings(Entity<SettingsPage>),
    Show(Entity<ShowPage>),
    Player(Entity<PlayerView>),
}

struct Entry {
    route: Route,
    page: Page,
}

enum Phase {
    Starting,
    Failed(SharedString),
    Ready,
}

/// The window's root view: sidebar, the current page and a back stack.
pub struct Root {
    phase: Phase,
    /// Current page last; never empty once ready.
    stack: Vec<Entry>,
    home: Option<Entity<HomePage>>,
    search: Option<Entity<SearchPage>>,
    toasts: Entity<Toasts>,
    focus: FocusHandle,
}

struct Started {
    db: Arc<Mutex<Db>>,
    settings: Settings,
    downloads: Arc<DownloadManager>,
    cache_dir: PathBuf,
    shims: Result<PathBuf, String>,
}

/// Opens the database, loads settings and downloads, writes ani-cli's shims.
fn start() -> Result<Started> {
    let db = Db::open(&paths::db_path())?;
    let settings = db.load_settings().unwrap_or_else(|err| {
        eprintln!("ani-gui: loading settings: {err:#}");
        Settings::default()
    });
    let db = Arc::new(Mutex::new(db));
    let downloads = Arc::new(DownloadManager::new(db.clone()));
    let cache_dir = paths::cache_dir();
    let shims = env::current_exe()
        .and_then(|exe| anicli::ensure_shims(&cache_dir, &exe))
        .map_err(|err| format!("Could not write ani-cli helper scripts: {err}"));
    if let Err(err) = &shims {
        eprintln!("ani-gui: {err}");
    }
    Ok(Started {
        db,
        settings,
        downloads,
        cache_dir,
        shims,
    })
}

impl Root {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let toasts = cx.new(|_| Toasts::default());
        cx.set_global(toast::ToastHost(toasts.clone()));
        let focus = cx.focus_handle();
        window.focus(&focus);
        let started = cx.background_spawn(async { start() });
        cx.spawn(async move |this, cx| {
            let started = started.await;
            this.update(cx, |root, cx| root.finish_start(started, cx))
                .ok();
        })
        .detach();
        Root {
            phase: Phase::Starting,
            stack: Vec::new(),
            home: None,
            search: None,
            toasts,
            focus,
        }
    }

    fn finish_start(&mut self, started: Result<Started>, cx: &mut Context<Self>) {
        let started = match started {
            Ok(started) => started,
            Err(err) => {
                eprintln!("ani-gui: could not open the library: {err:#}");
                self.phase = Phase::Failed(format!("{err:#}").into());
                cx.notify();
                return;
            }
        };
        let downloads_changed = cx.new(|_| DownloadsChanged);
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();
        started.downloads.set_on_change(Box::new(move || {
            let _ = tx.unbounded_send(());
        }));
        let watch = downloads_changed.clone();
        cx.spawn(async move |_, cx| {
            while rx.next().await.is_some() {
                while rx.try_recv().is_ok() {}
                if watch.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();
        let downloads = started.downloads.clone();
        // gpui waits only 100 ms for quit futures, too short for shutdown()'s
        // grace period before it kills stuck ani-cli processes.
        cx.on_app_quit(move |_, _| {
            anicli::cancel_all_resolves();
            downloads.shutdown();
            async {}
        })
        .detach();
        cx.set_global(AppState {
            db: started.db,
            settings: started.settings,
            downloads: started.downloads,
            cache_dir: started.cache_dir,
            shims: started.shims,
            downloads_changed,
        });
        self.phase = Phase::Ready;
        self.navigate(Route::Home, cx);
    }

    /// Opens `route`. Sidebar destinations start a fresh back stack; Show and
    /// Player pages stack on top of the current one, but never a player on
    /// a player.
    pub fn navigate(&mut self, route: Route, cx: &mut Context<Self>) {
        if matches!(route, Route::Player(_))
            && matches!(self.stack.last().map(|e| &e.page), Some(Page::Player(_)))
        {
            return;
        }
        let root = cx.entity().downgrade();
        let page = match &route {
            Route::Home => match &self.home {
                Some(home) => {
                    home.update(cx, |home, cx| home.refresh(cx));
                    Page::Home(home.clone())
                }
                None => {
                    let home = cx.new(|cx| HomePage::new(root, cx));
                    self.home = Some(home.clone());
                    Page::Home(home)
                }
            },
            Route::Search => {
                let search = self
                    .search
                    .get_or_insert_with(|| cx.new(|cx| SearchPage::new(root, cx)))
                    .clone();
                search.update(cx, |search, _| search.focus_on_next_render());
                Page::Search(search)
            }
            Route::Show(show) => {
                let show = show.clone();
                Page::Show(cx.new(|cx| ShowPage::new(show, root, cx)))
            }
            Route::Player(req) => {
                let req = req.clone();
                Page::Player(cx.new(|cx| PlayerView::new(req, root, cx)))
            }
            Route::MyList => Page::MyList(cx.new(|cx| MyListPage::new(root, cx))),
            Route::Calendar => Page::Calendar(cx.new(|cx| CalendarPage::new(root, cx))),
            Route::Downloads => Page::Downloads(cx.new(|cx| DownloadsPage::new(root, cx))),
            Route::Settings => Page::Settings(cx.new(SettingsPage::new)),
        };
        if !matches!(route, Route::Show(_) | Route::Player(_)) {
            self.stack.clear();
        }
        self.stack.push(Entry { route, page });
        cx.notify();
    }

    /// Returns to the previous page, if any. The player saves its progress
    /// first and then calls `close_player`.
    pub fn back(&mut self, cx: &mut Context<Self>) {
        if let Some(Page::Player(player)) = self.stack.last().map(|e| &e.page) {
            player.clone().update(cx, |player, cx| player.leave(cx));
            return;
        }
        self.pop(cx);
    }

    /// Leaves the player page `id` if it is still the current page.
    pub fn close_player(&mut self, id: EntityId, cx: &mut Context<Self>) {
        let current = self.stack.last().map(|e| &e.page);
        if matches!(current, Some(Page::Player(player)) if player.entity_id() == id) {
            self.pop(cx);
        }
    }

    fn pop(&mut self, cx: &mut Context<Self>) {
        if self.stack.len() > 1 {
            self.stack.pop();
            match self.stack.last().map(|e| &e.page) {
                Some(Page::Home(home)) => home.update(cx, |home, cx| home.refresh(cx)),
                Some(Page::Show(show)) => show.update(cx, |show, cx| show.reload(cx)),
                Some(Page::MyList(list)) => list.update(cx, |list, cx| list.reload(cx)),
                Some(Page::Downloads(downloads)) => {
                    downloads.update(cx, |downloads, cx| downloads.sync(cx))
                }
                _ => {}
            }
            cx.notify();
        }
    }

    /// Opens the player for `req` if page `from` is still the one showing:
    /// a play that took a while must not cover wherever the user went since.
    pub fn play_from(&mut self, from: EntityId, req: PlayRequest, cx: &mut Context<Self>) {
        if self.is_current(from) {
            self.navigate(Route::Player(req), cx);
        }
    }

    /// Whether the page entity `id` is the one showing.
    pub fn is_current(&self, id: EntityId) -> bool {
        let Some(entry) = self.stack.last() else {
            return false;
        };
        let current = match &entry.page {
            Page::Home(p) => p.entity_id(),
            Page::Search(p) => p.entity_id(),
            Page::MyList(p) => p.entity_id(),
            Page::Calendar(p) => p.entity_id(),
            Page::Downloads(p) => p.entity_id(),
            Page::Settings(p) => p.entity_id(),
            Page::Show(p) => p.entity_id(),
            Page::Player(p) => p.entity_id(),
        };
        current == id
    }

    fn current(&self) -> Option<&Route> {
        self.stack.last().map(|e| &e.route)
    }

    fn render_page(&self) -> gpui::AnyElement {
        let Some(entry) = self.stack.last() else {
            return div().into_any_element();
        };
        match &entry.page {
            Page::Home(home) => home.clone().into_any_element(),
            Page::Search(search) => search.clone().into_any_element(),
            Page::MyList(list) => list.clone().into_any_element(),
            Page::Calendar(calendar) => calendar.clone().into_any_element(),
            Page::Downloads(downloads) => downloads.clone().into_any_element(),
            Page::Settings(settings) => settings.clone().into_any_element(),
            Page::Show(show) => show.clone().into_any_element(),
            Page::Player(player) => player.clone().into_any_element(),
        }
    }
}

fn placeholder(title: &str, detail: String) -> gpui::AnyElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .justify_center()
        .px(theme::gutter())
        .gap_3()
        .child(
            div()
                .font_family(theme::DISPLAY_FONT)
                .font_weight(gpui::FontWeight::BOLD)
                .text_size(px(theme::PAGE_TITLE))
                .line_height(px(theme::PAGE_TITLE))
                .text_color(theme::text())
                .child(title.to_string()),
        )
        .child(
            div()
                .text_size(px(theme::BODY))
                .text_color(theme::text_dim())
                .child(detail),
        )
        .into_any_element()
}

impl Render for Root {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.phase {
            Phase::Starting => div().size_full().into_any_element(),
            Phase::Failed(err) => placeholder(
                "Couldn't open your library",
                format!("{err}\nDatabase: {}", paths::db_path().display()),
            ),
            Phase::Ready if matches!(self.current(), Some(Route::Player(_))) => self.render_page(),
            Phase::Ready => {
                let active = self.current().cloned().unwrap_or(Route::Home);
                div()
                    .size_full()
                    .flex()
                    .child(sidebar::sidebar(&active, cx))
                    .child(
                        div()
                            .flex_1()
                            .h_full()
                            .min_w_0()
                            .overflow_hidden()
                            .child(self.render_page()),
                    )
                    .into_any_element()
            }
        };
        div()
            .id("root")
            .key_context("Root")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Back, _, cx| this.back(cx)))
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Back),
                cx.listener(|this, _, _, cx| this.back(cx)),
            )
            .relative()
            .size_full()
            .bg(theme::night())
            .font_family(theme::UI_FONT)
            .text_size(px(theme::BODY))
            .text_color(theme::text())
            .child(body)
            .child(self.toasts.clone())
    }
}
