// Most of the model/store API has no caller until the UI views exist.
#[allow(dead_code)]
mod allanime;
#[allow(dead_code)]
mod anicli;
#[allow(dead_code)]
mod anilist;
#[allow(dead_code)]
mod catalog;
#[allow(dead_code)]
mod db;
#[allow(dead_code)]
mod downloads;
#[allow(dead_code)]
mod history;
#[allow(dead_code)]
mod images;
#[allow(dead_code)]
mod model;
#[allow(dead_code)]
mod net;
#[allow(dead_code)]
mod paths;
#[allow(dead_code)]
mod player;
#[allow(dead_code)]
mod progress;

use gpui::{
    App, Application, Bounds, Context, IntoElement, ParentElement, Render, SharedString, Styled,
    TitlebarOptions, Window, WindowBounds, WindowOptions, div, prelude::*, px, rgb, size,
};

use std::env;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use db::Db;

const APP_ID: &str = "io.github.sweettooth05.AniGui";

struct Root {
    _db: Option<Db>,
    db_error: Option<SharedString>,
}

impl Render for Root {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .bg(rgb(0x0e0e12))
            .text_color(rgb(0xf2f2f5))
            .child(div().text_2xl().child("ani-gui"))
            .child(
                div()
                    .text_color(rgb(0x8a8a96))
                    .child("Native client for ani-cli"),
            )
            .children(
                self.db_error
                    .clone()
                    .map(|err| div().text_color(rgb(0xff6b6b)).child(err)),
            )
    }
}

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

    let (db, db_error) = match Db::open(&paths::db_path()) {
        Ok(db) => (Some(db), None),
        Err(err) => {
            eprintln!("ani-gui: could not open database: {err:#}");
            (
                None,
                Some(format!("Could not open database: {err:#}").into()),
            )
        }
    };

    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("ani-gui".into()),
                ..Default::default()
            }),
            app_id: Some(APP_ID.into()),
            ..Default::default()
        };
        let opened = cx.open_window(options, |_, cx| cx.new(|_| Root { _db: db, db_error }));
        if let Err(err) = opened {
            eprintln!("ani-gui: could not open window: {err:#}");
            cx.quit();
        }
    });
}
