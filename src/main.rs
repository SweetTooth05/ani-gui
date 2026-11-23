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
mod progress;

use gpui::{
    App, Application, Bounds, Context, IntoElement, ParentElement, Render, SharedString, Styled,
    TitlebarOptions, Window, WindowBounds, WindowOptions, div, prelude::*, px, rgb, size,
};

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

fn main() {
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
