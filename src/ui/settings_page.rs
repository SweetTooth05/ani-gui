//! Settings: playback and download defaults, and a check of what ani-gui
//! needs installed.

use std::path::{Path, PathBuf};
use std::sync::PoisonError;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui::{
    AnyElement, Context, Entity, FontWeight, IntoElement, ScrollHandle, SharedString, Subscription,
    Task, Window, div, prelude::*, px, relative,
};

use crate::app::AppState;
use crate::diagnostics::{self, Report};
use crate::model::{Mode, Settings};
use crate::ui::components::button::{ButtonKind, button, segmented};
use crate::ui::components::text_input::{Changed, TextInput};
use crate::ui::components::toast;
use crate::ui::files::{display_path, open_folder};
use crate::ui::show::QUALITIES;
use crate::ui::theme;

const CONTENT_W: f32 = 780.;

/// Bumped on every settings save, so an older save finishing late is
/// skipped.
static SAVE_GENERATION: AtomicU64 = AtomicU64::new(0);

pub struct SettingsPage {
    settings: Settings,
    dir_input: Entity<TextInput>,
    dir_error: Option<String>,
    saving_dir: bool,
    report: Option<Report>,
    check_task: Option<Task<()>>,
    scroll: ScrollHandle,
    _dir_changes: Subscription,
}

impl SettingsPage {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let settings = cx
            .try_global::<AppState>()
            .map(|s| s.settings.clone())
            .unwrap_or_default();
        let dir_input = cx.new(|cx| {
            let mut input = TextInput::new("Folder for downloads", cx);
            input.set_text(settings.download_dir.to_string_lossy().into_owned(), cx);
            input
        });
        let dir_changes = cx.subscribe(&dir_input, |page, _, _: &Changed, cx| {
            page.dir_error = None;
            cx.notify();
        });
        let mut page = SettingsPage {
            settings,
            dir_input,
            dir_error: None,
            saving_dir: false,
            report: None,
            check_task: None,
            scroll: ScrollHandle::new(),
            _dir_changes: dir_changes,
        };
        page.check(cx);
        page
    }

    fn check(&mut self, cx: &mut Context<Self>) {
        if self.check_task.is_some() {
            return;
        }
        self.check_task = Some(cx.spawn(async move |this, cx| {
            let report = cx.background_spawn(async { diagnostics::run() }).await;
            this.update(cx, |page, cx| {
                page.check_task = None;
                page.report = Some(report);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Makes `settings` current for everything opened from now on and
    /// saves it in the background.
    fn apply(&mut self, settings: Settings, cx: &mut Context<Self>) {
        let Some(state) = cx.try_global::<AppState>() else {
            return;
        };
        let db = state.db.clone();
        cx.global_mut::<AppState>().settings = settings.clone();
        self.settings = settings.clone();
        let mine = SAVE_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
        cx.notify();
        cx.spawn(async move |_, cx| {
            let saved = cx
                .background_spawn(async move {
                    let db = db.lock().unwrap_or_else(PoisonError::into_inner);
                    if SAVE_GENERATION.load(Ordering::SeqCst) != mine {
                        return Ok(());
                    }
                    db.save_settings(&settings)
                })
                .await;
            if let Err(err) = saved {
                cx.update(|cx| toast::error(cx, format!("Couldn't save settings: {err:#}")))
                    .ok();
            }
        })
        .detach();
    }

    fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if self.settings.mode != mode {
            let settings = Settings {
                mode,
                ..self.settings.clone()
            };
            self.apply(settings, cx);
        }
    }

    fn set_quality(&mut self, quality: &str, cx: &mut Context<Self>) {
        if self.settings.quality != quality {
            let settings = Settings {
                quality: quality.to_string(),
                ..self.settings.clone()
            };
            self.apply(settings, cx);
        }
    }

    fn toggle_autoplay(&mut self, cx: &mut Context<Self>) {
        let settings = Settings {
            autoplay_next: !self.settings.autoplay_next,
            ..self.settings.clone()
        };
        self.apply(settings, cx);
    }

    /// The folder typed in, if it differs from the current one.
    fn typed_dir(&self, cx: &Context<Self>) -> Option<String> {
        let typed = self.dir_input.read(cx).text().trim().to_string();
        (Path::new(&typed) != self.settings.download_dir).then_some(typed)
    }

    /// Checks the typed folder (creating it, and writing a file in it),
    /// then makes it the download folder.
    fn save_dir(&mut self, cx: &mut Context<Self>) {
        let Some(typed) = self.typed_dir(cx) else {
            return;
        };
        let dir = match parse_dir(&typed, std::env::home_dir().as_deref()) {
            Ok(dir) => dir,
            Err(err) => {
                self.dir_error = Some(err);
                cx.notify();
                return;
            }
        };
        self.saving_dir = true;
        self.dir_error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let made = {
                let dir = dir.clone();
                cx.background_spawn(async move { prepare_dir(&dir) }).await
            };
            this.update(cx, |page, cx| {
                page.saving_dir = false;
                match made {
                    Ok(()) => {
                        page.dir_input.update(cx, |input, cx| {
                            input.set_text(dir.to_string_lossy().into_owned(), cx)
                        });
                        let settings = Settings {
                            download_dir: dir,
                            ..page.settings.clone()
                        };
                        page.apply(settings, cx);
                    }
                    Err(err) => page.dir_error = Some(format!("Can't use that folder: {err}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_playback(&self, cx: &mut Context<Self>) -> AnyElement {
        let mode = self.settings.mode;
        let mut quality = segmented("settings-quality");
        for (value, label) in QUALITIES {
            quality = quality.option(
                label,
                self.settings.quality == value,
                cx.listener(move |page, _, _, cx| page.set_quality(value, cx)),
            );
        }
        let on = self.settings.autoplay_next;
        section("Playback")
            .child(setting_row(
                "Audio",
                "Sub or Dub, for shows you open from now on.",
                segmented("settings-mode")
                    .option(
                        "Sub",
                        mode == Mode::Sub,
                        cx.listener(|page, _, _, cx| page.set_mode(Mode::Sub, cx)),
                    )
                    .option(
                        "Dub",
                        mode == Mode::Dub,
                        cx.listener(|page, _, _, cx| page.set_mode(Mode::Dub, cx)),
                    )
                    .into_any_element(),
            ))
            .child(setting_row(
                "Quality",
                "For streams and downloads. A show's page can pick another.",
                quality.into_any_element(),
            ))
            .child(setting_row(
                "Autoplay next episode",
                "Starts the next episode a few seconds after one ends.",
                div()
                    .id("settings-autoplay")
                    .flex()
                    .flex_none()
                    .items_center()
                    .w(px(48.))
                    .h(px(28.))
                    .p(px(3.))
                    .rounded_full()
                    .cursor_pointer()
                    .bg(if on { theme::accent() } else { theme::raised() })
                    .when(on, |d| d.justify_end())
                    .on_click(cx.listener(|page, _, _, cx| page.toggle_autoplay(cx)))
                    .child(div().size(px(22.)).rounded_full().bg(if on {
                        theme::on_accent()
                    } else {
                        theme::text_dim()
                    }))
                    .into_any_element(),
            ))
            .into_any_element()
    }

    fn render_downloads(&self, cx: &mut Context<Self>) -> AnyElement {
        let changed = self.typed_dir(cx).is_some();
        let current = self.settings.download_dir.clone();
        section("Downloads")
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .py(px(18.))
                    .border_b_1()
                    .border_color(theme::line())
                    .child(label_block(
                        "Download folder",
                        "Where downloaded episodes are saved. Type a full path and press Save.",
                    ))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(div().flex_1().min_w_0().child(self.dir_input.clone()))
                            .child(
                                button(
                                    "settings-dir-save",
                                    if self.saving_dir { "Saving…" } else { "Save" },
                                )
                                .kind(ButtonKind::Secondary)
                                .disabled(!changed || self.saving_dir)
                                .on_click(cx.listener(|page, _, _, cx| page.save_dir(cx))),
                            )
                            .child(
                                button("settings-dir-open", "Open")
                                    .kind(ButtonKind::Ghost)
                                    .on_click(move |_, _, cx| open_folder(current.clone(), cx)),
                            ),
                    )
                    .children(self.dir_error.clone().map(|err| {
                        div()
                            .text_size(px(theme::SMALL))
                            .text_color(theme::danger())
                            .child(err)
                    })),
            )
            .into_any_element()
    }

    fn render_diagnostics(&self, cx: &mut Context<Self>) -> AnyElement {
        let heading = div()
            .flex()
            .items_center()
            .justify_between()
            .child(section_title("Diagnostics"))
            .child(
                button(
                    "settings-recheck",
                    if self.check_task.is_some() {
                        "Checking…"
                    } else {
                        "Check again"
                    },
                )
                .kind(ButtonKind::Ghost)
                .compact()
                .disabled(self.check_task.is_some())
                .on_click(cx.listener(|page, _, _, cx| page.check(cx))),
            );
        let body = div().flex().flex_col();
        let Some(report) = &self.report else {
            return div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(heading)
                .child(
                    body.py(px(18.))
                        .text_color(theme::text_muted())
                        .child("Checking what's installed…"),
                )
                .into_any_element();
        };
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(heading)
            .child(body.children(checks(report).into_iter().map(check_row)))
            .into_any_element()
    }
}

impl Render for SettingsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("settings")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(40.))
                    .max_w(px(CONTENT_W) + theme::gutter() * 2.)
                    .px(theme::gutter())
                    .pt(px(48.))
                    .pb(px(64.))
                    .child(
                        div()
                            .font_family(theme::DISPLAY_FONT)
                            .font_weight(FontWeight::BOLD)
                            .text_size(px(theme::PAGE_TITLE))
                            .line_height(px(theme::PAGE_TITLE))
                            .child("Settings"),
                    )
                    .child(self.render_playback(cx))
                    .child(self.render_downloads(cx))
                    .child(self.render_diagnostics(cx)),
            )
    }
}

/// One line of the diagnostics list.
struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
}

fn checks(report: &Report) -> Vec<Check> {
    let tool = |name: &'static str, found: bool, needed_for: &str| Check {
        name,
        ok: found,
        detail: if found {
            "Found on your PATH".into()
        } else {
            format!("Not found on your PATH. ani-cli needs it {needed_for}.")
        },
    };
    vec![
        Check {
            name: "ani-cli",
            ok: report.ani_cli.is_some(),
            detail: match &report.ani_cli {
                Some((path, Some(version))) => {
                    format!("Version {version} at {}", display_path(path))
                }
                Some((path, None)) => format!(
                    "Found at {}, but it didn't report a version",
                    display_path(path)
                ),
                None => "Not found. Install ani-cli to stream and download episodes.".into(),
            },
        },
        Check {
            name: "libmpv",
            ok: report.libmpv.is_ok(),
            detail: match &report.libmpv {
                Ok(()) => "Loaded, so videos play in this window".into(),
                Err(err) => format!(
                    "Couldn't load it ({err}). Install libmpv2 (sudo apt install libmpv2) to play videos."
                ),
            },
        },
        tool("yt-dlp", report.ytdlp, "to download some episodes"),
        tool("ffmpeg", report.ffmpeg, "to download some episodes"),
        tool("curl", report.curl, "to find episodes"),
    ]
}

fn check_row(check: Check) -> impl IntoElement {
    div()
        .flex()
        .items_start()
        .gap(px(16.))
        .py(px(14.))
        .border_b_1()
        .border_color(theme::line())
        .child(
            div()
                .flex_none()
                .w(px(96.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(check.name),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(theme::SMALL))
                .line_height(relative(1.5))
                .text_color(if check.ok {
                    theme::text_dim()
                } else {
                    theme::danger()
                })
                .child(check.detail),
        )
        .child(
            div()
                .flex_none()
                .text_size(px(theme::SMALL))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(if check.ok {
                    theme::text_dim()
                } else {
                    theme::danger()
                })
                .child(if check.ok { "Ready" } else { "Missing" }),
        )
}

fn section_title(title: &'static str) -> impl IntoElement {
    div()
        .font_family(theme::DISPLAY_FONT)
        .font_weight(FontWeight::SEMIBOLD)
        .text_size(px(theme::SECTION))
        .child(title)
}

fn section(title: &'static str) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(section_title(title))
}

fn label_block(label: &'static str, help: impl Into<SharedString>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .min_w_0()
        .child(div().font_weight(FontWeight::SEMIBOLD).child(label))
        .child(
            div()
                .text_size(px(theme::SMALL))
                .line_height(relative(1.45))
                .text_color(theme::text_muted())
                .child(help.into()),
        )
}

fn setting_row(label: &'static str, help: &'static str, control: AnyElement) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(32.))
        .py(px(18.))
        .border_b_1()
        .border_color(theme::line())
        .child(div().flex_1().min_w_0().child(label_block(label, help)))
        .child(control)
}

/// The download folder typed by the user: absolute, with "~" for `home`.
fn parse_dir(typed: &str, home: Option<&Path>) -> Result<PathBuf, String> {
    let typed = typed.trim();
    let dir = match (typed.strip_prefix('~'), home) {
        (Some(""), Some(home)) => home.to_path_buf(),
        (Some(rest), Some(home)) if rest.starts_with('/') => {
            home.join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(typed),
    };
    if typed.is_empty() {
        return Err("Type the folder to save downloads in.".into());
    }
    if !dir.is_absolute() {
        return Err("Use a full path, starting with / or ~/.".into());
    }
    Ok(dir)
}

/// Creates `dir` if needed and checks a file can be written in it.
fn prepare_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let probe = dir.join(format!(".ani-gui-write-test-{}", std::process::id()));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)?;
    std::fs::remove_file(&probe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_dir_needs_a_writable_folder() {
        let dir = std::env::temp_dir().join(format!("ani-gui-prep-{}", std::process::id()));
        let nested = dir.join("a/b");
        prepare_dir(&nested).unwrap();
        assert!(nested.is_dir());
        // The probe file is gone afterwards.
        assert_eq!(std::fs::read_dir(&nested).unwrap().count(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
        // System folders aren't writable by a normal user.
        // SAFETY: geteuid has no preconditions.
        if unsafe { nix::libc::geteuid() } != 0 {
            assert!(prepare_dir(Path::new("/")).is_err());
            assert!(prepare_dir(Path::new("/usr")).is_err());
        }
    }

    #[test]
    fn parse_dir_expands_home_and_needs_full_paths() {
        let home = Path::new("/home/k");
        assert_eq!(
            parse_dir("~/Videos/anime ", Some(home)),
            Ok(PathBuf::from("/home/k/Videos/anime"))
        );
        assert_eq!(parse_dir("~", Some(home)), Ok(PathBuf::from("/home/k")));
        assert_eq!(
            parse_dir("/mnt/media", Some(home)),
            Ok(PathBuf::from("/mnt/media"))
        );
        assert!(parse_dir("Videos", Some(home)).is_err());
        assert!(parse_dir("  ", Some(home)).is_err());
        assert!(parse_dir("~other/x", Some(home)).is_err());
    }
}
