mod allanime;
mod anicli;
mod anilist;
mod app;
mod catalog;
mod db;
mod diagnostics;
mod downloads;
mod history;
mod images;
mod keep_awake;
mod lists;
mod model;
mod net;
mod opener;
mod paths;
mod playback;
mod player;
mod progress;
mod reviews;
mod ui;
mod updater;

use gpui::{
    App, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, prelude::*, px, size,
};

use std::env;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use ui::components::text_input;

const APP_ID: &str = "io.github.sweettooth05.AniGui";

/// ani-cli's menu program (see `anicli::ensure_shims`): answers one menu on
/// stdout and exits without starting the UI.
fn run_ani_cli_menu(args: &[String]) {
    let mut input = Vec::new();
    if let Err(err) = io::stdin().read_to_end(&mut input) {
        eprintln!("ani-gui: reading menu input: {err}");
    }
    let want_titles: Vec<String> = env::var("ANI_GUI_WANT_TITLES")
        .unwrap_or_default()
        .lines()
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect();
    let out_file = env::var_os("ANI_GUI_MENU_OUT")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from);
    let picked = anicli::menu_main(
        args,
        &String::from_utf8_lossy(&input),
        &want_titles,
        out_file.as_deref(),
    );
    let mut stdout = io::stdout();
    if let Err(err) = stdout
        .write_all(picked.as_bytes())
        .and_then(|()| stdout.flush())
    {
        eprintln!("ani-gui: writing menu choice: {err}");
    }
}

fn main() {
    let args: Vec<String> = env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    if args.get(1).map(String::as_str) == Some(anicli::MENU_FLAG) {
        run_ani_cli_menu(&args[2..]);
        return;
    }

    let app = Application::new().with_assets(ui::theme::Assets);
    app.run(|cx: &mut App| {
        ui::theme::register_fonts(cx);
        text_input::bind_keys(cx);
        app::bind_keys(cx);
        let bounds = Bounds::centered(None, size(px(1360.), px(860.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("ani-gui".into()),
                ..Default::default()
            }),
            app_id: Some(APP_ID.into()),
            window_min_size: Some(size(px(960.), px(600.))),
            ..Default::default()
        };
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let opened = cx.open_window(options, |window, cx| {
            cx.new(|cx| app::Root::new(window, cx))
        });
        if let Err(err) = opened {
            eprintln!("ani-gui: could not open window: {err:#}");
            cx.quit();
        }
    });
}
