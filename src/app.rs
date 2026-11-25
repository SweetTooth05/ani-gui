use std::env;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use futures::StreamExt;
use gpui::{
    App, Context, Entity, EntityId, FocusHandle, Global, KeyBinding, Menu, MenuItem, MouseButton,
    NavigationDirection, SharedString, SystemMenuType, Window, actions, div, prelude::*, px,
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
use crate::ui::{sidebar, theme, titlebar};
use crate::{anicli, paths};

/// Process-start timestamp when `ANI_GUI_TRACE` is set (non-empty and not "0").
static TRACE_START: OnceLock<Option<Instant>> = OnceLock::new();

pub fn trace_init() {
    let _ = TRACE_START.set(
        env::var_os("ANI_GUI_TRACE")
            .filter(|v| !v.is_empty() && v != "0")
            .map(|_| Instant::now()),
    );
}

#[inline]
pub fn trace_enabled() -> bool {
    TRACE_START.get().is_some_and(Option::is_some)
}

#[inline]
pub fn trace_event(label: &str) {
    if let Some(Some(start)) = TRACE_START.get() {
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        eprintln!("[ani-gui trace +{ms:7.2}ms] {label}");
    }
}

#[inline]
pub fn trace_span(label: &str, dur: Duration) {
    if let Some(Some(start)) = TRACE_START.get() {
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        let dur_ms = dur.as_secs_f64() * 1000.0;
        eprintln!("[ani-gui trace +{ms:7.2}ms] {label} ({dur_ms:.2}ms)");
    }
}

actions!(
    ani_gui,
    [
        Back,
        Quit,
        CloseWindow,
        ToggleFullscreen,
        Hide,
        HideOthers,
        ShowAll,
        Minimize,
        Zoom
    ]
);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("alt-left", Back, None),
        KeyBinding::new("f11", ToggleFullscreen, None),
    ]);
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &CloseWindow, cx| {
        if let Some(window) = cx.active_window() {
            window
                .update(cx, |_, window, _| window.remove_window())
                .ok();
        }
    });
    cx.on_action(|_: &ToggleFullscreen, cx| {
        if let Some(window) = cx.active_window() {
            window
                .update(cx, |_, window, _| window.toggle_fullscreen())
                .ok();
        }
    });
    player_view::bind_keys(cx);
    if cfg!(target_os = "linux") {
        cx.bind_keys([
            KeyBinding::new("ctrl-q", Quit, None),
            KeyBinding::new("ctrl-w", CloseWindow, None),
        ]);
    }
    if cfg!(target_os = "macos") {
        install_mac_menus(cx);
    }
}

/// The standard macOS application and Window menus with their shortcuts.
/// GPUI provides none of them, not even ⌘Q.
fn install_mac_menus(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-[", Back, None),
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-h", Hide, None),
        KeyBinding::new("alt-cmd-h", HideOthers, None),
        KeyBinding::new("cmd-m", Minimize, None),
    ]);
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    cx.on_action(|_: &Minimize, cx| {
        if let Some(window) = cx.active_window() {
            window
                .update(cx, |_, window, _| window.minimize_window())
                .ok();
        }
    });
    cx.on_action(|_: &Zoom, cx| {
        if let Some(window) = cx.active_window() {
            window.update(cx, |_, window, _| window.zoom_window()).ok();
        }
    });
    cx.set_menus(vec![
        Menu {
            name: "ani-gui".into(),
            items: vec![
                MenuItem::os_submenu("Services", SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action("Hide ani-gui", Hide),
                MenuItem::action("Hide Others", HideOthers),
                MenuItem::action("Show All", ShowAll),
                MenuItem::separator(),
                MenuItem::action("Quit ani-gui", Quit),
            ],
        },
        Menu {
            name: "Window".into(),
            items: vec![
                MenuItem::action("Minimize", Minimize),
                MenuItem::action("Zoom", Zoom),
            ],
        },
    ]);
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
    home_snapshot: Option<crate::ui::home::HomeData>,
}

/// Opens the database, loads settings and downloads, reads the cached Home
/// snapshot, and writes ani-cli's shims.
fn start() -> Result<Started> {
    let t0 = Instant::now();
    let db = Db::open(&paths::db_path())?;
    trace_span("start.db_open", t0.elapsed());
    let t1 = Instant::now();
    let settings = db.load_settings().unwrap_or_else(|err| {
        eprintln!("ani-gui: loading settings: {err:#}");
        Settings::default()
    });
    trace_span("start.load_settings", t1.elapsed());
    let t_snap = Instant::now();
    let home_snapshot = crate::ui::home::read_snapshot_db(&db);
    trace_span(
        if home_snapshot.is_some() {
            "start.home_snapshot.hit"
        } else {
            "start.home_snapshot.miss"
        },
        t_snap.elapsed(),
    );
    let db = Arc::new(Mutex::new(db));
    let t2 = Instant::now();
    let downloads = Arc::new(DownloadManager::new(db.clone()));
    trace_span("start.downloads_new", t2.elapsed());
    let cache_dir = paths::cache_dir();
    let t3 = Instant::now();
    let shims = env::current_exe()
        .and_then(|exe| anicli::ensure_shims(&cache_dir, &exe))
        .map_err(|err| format!("Could not write ani-cli helper scripts: {err}"));
    trace_span("start.ensure_shims", t3.elapsed());
    if let Err(err) = &shims {
        eprintln!("ani-gui: {err}");
    }
    trace_span("start.total", t0.elapsed());
    Ok(Started {
        db,
        settings,
        downloads,
        cache_dir,
        shims,
        home_snapshot,
    })
}

impl Root {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        trace_event("root_new");
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
        trace_event("root_finish_start");
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
        let root = cx.entity().downgrade();
        // Register on `App`, not `Context<Root>`: closing the last window drops
        // `Root` before `on_app_quit` fires, and gpui waits only 100 ms for
        // quit futures (too short for shutdown()'s grace period).
        App::on_app_quit(cx, move |cx| {
            root.update(cx, |root, cx| root.save_player_for_quit(cx))
                .ok();
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
        let root = cx.entity().downgrade();
        let home = cx.new(|cx| HomePage::new_with_snapshot(root, started.home_snapshot, cx));
        self.home = Some(home.clone());
        self.stack.push(Entry {
            route: Route::Home,
            page: Page::Home(home),
        });
        cx.notify();
        crate::updater::init(cx);
        crate::ui::downloads_page::watch_toasts(cx);
        crate::ui::accounts::start(cx);
    }

    /// Opens `route`. Sidebar destinations start a fresh back stack; Show and
    /// Player pages stack on top of the current one, but never a player on
    /// a player.
    pub fn navigate(&mut self, route: Route, cx: &mut Context<Self>) {
        if !matches!(self.phase, Phase::Ready) {
            return;
        }
        if matches!(route, Route::Player(_))
            && matches!(self.stack.last().map(|e| &e.page), Some(Page::Player(_)))
        {
            return;
        }
        crate::ui::components::poster::clear_hovered_poster(cx);
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
            crate::ui::components::poster::clear_hovered_poster(cx);
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

    /// Saves playback progress synchronously if the player is open on quit.
    fn save_player_for_quit(&mut self, cx: &mut Context<Self>) {
        for entry in &self.stack {
            if let Page::Player(player) = &entry.page {
                player.update(cx, |player, _| player.save_for_quit());
            }
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        static FIRST_FRAME: OnceLock<()> = OnceLock::new();
        if trace_enabled() && FIRST_FRAME.set(()).is_ok() {
            trace_event("root_first_render");
        }
        let body = match &self.phase {
            Phase::Starting => div()
                .size_full()
                .flex()
                .child(sidebar::sidebar(&Route::Home, cx))
                .child(
                    div()
                        .flex_1()
                        .h_full()
                        .min_w_0()
                        .overflow_hidden()
                        .child(crate::ui::home::skeleton()),
                )
                .into_any_element(),
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
        let titlebar = titlebar::bar(window);
        let root = div()
            .id("root")
            .key_context("Root")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Back, _, cx| this.back(cx)))
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Back),
                cx.listener(|this, _, _, cx| this.back(cx)),
            )
            .size_full()
            .flex()
            .flex_col()
            .font_family(theme::UI_FONT)
            .text_size(px(theme::BODY))
            .text_color(theme::text())
            .children(titlebar)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_hidden()
                    .child(body)
                    .child(self.toasts.clone()),
            );
        titlebar::frame(root, window, cx)
    }
}
