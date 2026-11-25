//! Downloads: every download with live progress, and what to do with it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use gpui::{
    AnyElement, App, Context, FontWeight, IntoElement, ScrollHandle, SharedString, Subscription,
    Task, WeakEntity, Window, div, prelude::*, px, relative,
};

use crate::app::{AppState, PlayRequest, Root, Route};
use crate::db::{Db, lock};
use crate::downloads::{DlProgress, DownloadEvent, DownloadManager};
use crate::model::{DlStatus, DownloadRecord, ShowSummary};
use crate::playback;
use crate::ui::components::button::{ButtonKind, button};
use crate::ui::components::image::rounded_image;
use crate::ui::components::poster::progress_bar;
use crate::ui::components::toast;
use crate::ui::files::{display_path, open_folder};
use crate::ui::theme;
use crate::ui::view_models::mode_label;

const COVER_W: f32 = 64.;
const COVER_H: f32 = 90.;

/// Subscribes to `DownloadManager` lifecycle events and turns them into
/// coalesced toasts (started, finished, failed) regardless of the active page.
pub fn watch_toasts(cx: &mut App) {
    let Some(state) = cx.try_global::<AppState>() else {
        return;
    };
    let watch = state.downloads_changed.clone();
    let downloads = state.downloads.clone();
    cx.observe(&watch, move |_, cx| {
        for event in downloads.take_events() {
            emit_download_toast(event, cx);
        }
    })
    .detach();
}

fn emit_download_toast(event: DownloadEvent, cx: &mut App) {
    match event {
        DownloadEvent::Started { id, title, ep } => {
            toast::Toast::info(format!("Downloading {title} episode {ep}"))
                .key(format!("download:{id}"))
                .show(cx);
        }
        DownloadEvent::Finished {
            id,
            title,
            ep,
            path,
        } => {
            let mut t = toast::Toast::success(format!("Downloaded {title} episode {ep}"))
                .key(format!("download:{id}"));
            if let Some(folder) = path.parent().map(Path::to_path_buf) {
                t = t.action("Open folder", move |_, cx| {
                    open_folder(folder.clone(), cx);
                });
            }
            t.show(cx);
        }
        DownloadEvent::Failed {
            id,
            title,
            ep,
            error,
        } => {
            let summary = short_error(&error);
            let message = if summary.is_empty() {
                format!("Download failed: {title} episode {ep}")
            } else {
                format!("Download failed: {title} ep {ep} — {summary}")
            };
            toast::Toast::error(message)
                .key(format!("download:{id}"))
                .show(cx);
        }
    }
}

fn short_error(error: &str) -> String {
    let line = error.lines().next().unwrap_or(error).trim();
    const LIMIT: usize = 80;
    if line.chars().count() <= LIMIT {
        line.to_string()
    } else {
        let mut out: String = line.chars().take(LIMIT - 1).collect();
        out.push('…');
        out
    }
}

struct Row {
    record: DownloadRecord,
    live: Option<DlProgress>,
    /// For finished downloads: the file is still where it was saved.
    file_exists: bool,
}

/// The list sections, in display order.
const SECTIONS: [(&str, &[DlStatus]); 3] = [
    ("Downloading", &[DlStatus::Running, DlStatus::Queued]),
    ("Ready to watch", &[DlStatus::Done]),
    ("Didn't finish", &[DlStatus::Failed, DlStatus::Cancelled]),
];

pub struct DownloadsPage {
    root: WeakEntity<Root>,
    rows: Vec<Row>,
    /// Cover art by show id, read from the library once per show.
    covers: HashMap<String, Option<String>>,
    loaded: bool,
    /// Downloads with an action in flight.
    busy: HashSet<String>,
    sync_task: Option<Task<()>>,
    /// The list changed while `sync_task` was reading it.
    sync_again: bool,
    scroll: ScrollHandle,
    _downloads_changed: Option<Subscription>,
}

impl DownloadsPage {
    pub fn new(root: WeakEntity<Root>, cx: &mut Context<Self>) -> Self {
        let watch = cx
            .try_global::<AppState>()
            .map(|s| s.downloads_changed.clone());
        let subscription = watch.map(|w| cx.observe(&w, |page, _, cx| page.changed(cx)));
        let mut page = DownloadsPage {
            root,
            rows: Vec::new(),
            covers: HashMap::new(),
            loaded: false,
            busy: HashSet::new(),
            sync_task: None,
            sync_again: false,
            scroll: ScrollHandle::new(),
            _downloads_changed: subscription,
        };
        page.sync(cx);
        page
    }

    /// The download list changed. A page covered by another (the player)
    /// skips it; `Root` syncs it when it is back on top.
    fn changed(&mut self, cx: &mut Context<Self>) {
        let id = cx.entity_id();
        let on_top = self
            .root
            .read_with(cx, |root, _| root.is_current(id))
            .unwrap_or(false);
        if on_top {
            self.sync(cx);
        }
    }

    /// Re-reads the manager's list (and any covers not yet known). While a
    /// read is running, asks for exactly one more after it instead of
    /// cancelling it, so a stream of progress updates can't starve the rows.
    pub fn sync(&mut self, cx: &mut Context<Self>) {
        if self.sync_task.is_some() {
            self.sync_again = true;
            return;
        }
        let Some(state) = cx.try_global::<AppState>() else {
            return;
        };
        let (db, manager) = (state.db.clone(), state.downloads.clone());
        let known: HashSet<String> = self.covers.keys().cloned().collect();
        self.sync_task = Some(cx.spawn(async move |this, cx| {
            let (rows, covers) = cx
                .background_spawn(async move { snapshot(&db, &manager, &known) })
                .await;
            this.update(cx, |page, cx| {
                page.sync_task = None;
                page.rows = rows;
                page.covers.extend(covers);
                page.loaded = true;
                if std::mem::take(&mut page.sync_again) {
                    page.sync(cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Runs `action` on the manager in the background, keeping `id`'s
    /// buttons disabled until it returns; an error becomes a toast.
    fn act(
        &mut self,
        id: &str,
        failure: &'static str,
        action: impl FnOnce(&Mutex<Db>, &DownloadManager, &Path) -> Result<()> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = cx.try_global::<AppState>() else {
            return;
        };
        if !self.busy.insert(id.to_string()) {
            return;
        }
        let (db, manager) = (state.db.clone(), state.downloads.clone());
        let dir = state.settings.download_dir.clone();
        let id = id.to_string();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let done = cx
                .background_spawn(async move { action(&db, &manager, &dir) })
                .await;
            this.update(cx, |page, cx| {
                page.busy.remove(&id);
                if let Err(err) = done {
                    toast::error(cx, format!("{failure}: {err:#}"));
                }
                page.sync(cx);
            })
            .ok();
        })
        .detach();
    }

    fn cancel(&mut self, id: String, cx: &mut Context<Self>) {
        let target = id.clone();
        self.act(
            &id,
            "Couldn't cancel the download",
            move |_, manager, _| {
                manager.cancel(&target);
                Ok(())
            },
            cx,
        );
    }

    fn remove(&mut self, id: String, cx: &mut Context<Self>) {
        let target = id.clone();
        self.act(
            &id,
            "Couldn't remove the download",
            move |_, manager, _| {
                manager.remove(&target);
                Ok(())
            },
            cx,
        );
    }

    fn retry(&mut self, record: DownloadRecord, cx: &mut Context<Self>) {
        let id = record.id.clone();
        self.act(
            &id,
            "Couldn't retry the download",
            move |db, manager, dir| retry_or_restart(db, manager, &record, dir),
            cx,
        );
    }

    /// Plays a finished download from disk, continuing through the show's
    /// other downloaded episodes.
    fn play(&mut self, record: DownloadRecord, cx: &mut Context<Self>) {
        let Some(state) = cx.try_global::<AppState>() else {
            return;
        };
        if !self.busy.insert(record.id.clone()) {
            return;
        }
        let (db, manager) = (state.db.clone(), state.downloads.clone());
        let id = record.id.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let req = cx
                .background_spawn(async move { play_request(&db, &manager, &record) })
                .await;
            this.update(cx, |page, cx| {
                page.busy.remove(&id);
                match req {
                    Ok(req) => {
                        let from = cx.entity_id();
                        page.root
                            .update(cx, |root, cx| root.play_from(from, req, cx))
                            .ok();
                    }
                    Err(err) => {
                        toast::error(cx, format!("Couldn't play the download: {err:#}"));
                        page.sync(cx);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_row(&self, index: usize, row: &Row, cx: &mut Context<Self>) -> AnyElement {
        let record = &row.record;
        let busy = self.busy.contains(&record.id);
        let cover = self.covers.get(&record.show_id).cloned().flatten();
        let art = div()
            .flex_none()
            .w(px(COVER_W))
            .h(px(COVER_H))
            .rounded(px(8.))
            .bg(theme::raised())
            .child(match cover {
                Some(url) => rounded_image(&url, px(8.), theme::night(), None).into_any_element(),
                None => div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .font_family(theme::DISPLAY_FONT)
                    .font_weight(FontWeight::BOLD)
                    .text_size(px(26.))
                    .text_color(theme::text_muted())
                    .child(record.ep.clone())
                    .into_any_element(),
            });
        let meta = div()
            .flex()
            .flex_wrap()
            .gap(px(14.))
            .text_size(px(theme::SMALL))
            .text_color(theme::text_dim())
            .child(format!("Episode {}", record.ep))
            .child(mode_label(record.mode))
            .child(quality_label(&record.quality));
        div()
            .id(("download-row", index))
            .flex()
            .items_center()
            .gap(px(20.))
            .p(px(10.))
            .rounded(px(12.))
            .child(art)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(record.title.clone()),
                    )
                    .child(meta)
                    .child(status_line(row)),
            )
            .child(self.render_actions(index, row, busy, cx))
            .into_any_element()
    }

    fn render_actions(
        &self,
        index: usize,
        row: &Row,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let record = &row.record;
        let id = record.id.clone();
        let mut actions = div().flex().flex_none().items_center().gap(px(6.));
        match record.status {
            DlStatus::Queued | DlStatus::Running => {
                actions = actions.child(
                    button(("download-cancel", index), "Cancel")
                        .kind(ButtonKind::Secondary)
                        .compact()
                        .disabled(busy)
                        .on_click(cx.listener(move |page, _, _, cx| page.cancel(id.clone(), cx))),
                );
            }
            DlStatus::Done => {
                let play = record.clone();
                let folder = record
                    .path
                    .as_deref()
                    .and_then(|p| Path::new(p).parent())
                    .map(Path::to_path_buf);
                actions = actions
                    .child(
                        button(("download-play", index), "Play")
                            .compact()
                            .disabled(busy || !row.file_exists)
                            .on_click(
                                cx.listener(move |page, _, _, cx| page.play(play.clone(), cx)),
                            ),
                    )
                    .children(folder.filter(|_| row.file_exists).map(|folder| {
                        button(("download-folder", index), "Open folder")
                            .kind(ButtonKind::Ghost)
                            .compact()
                            .on_click(move |_, _, cx| open_folder(folder.clone(), cx))
                    }))
                    .child(
                        button(("download-remove", index), "Remove from list")
                            .kind(ButtonKind::Ghost)
                            .compact()
                            .disabled(busy)
                            .on_click(
                                cx.listener(move |page, _, _, cx| page.remove(id.clone(), cx)),
                            ),
                    );
            }
            DlStatus::Failed | DlStatus::Cancelled => {
                let retry = record.clone();
                actions = actions
                    .child(
                        button(("download-retry", index), "Retry")
                            .kind(ButtonKind::Secondary)
                            .compact()
                            .disabled(busy)
                            .on_click(
                                cx.listener(move |page, _, _, cx| page.retry(retry.clone(), cx)),
                            ),
                    )
                    .child(
                        button(("download-remove", index), "Remove from list")
                            .kind(ButtonKind::Ghost)
                            .compact()
                            .disabled(busy)
                            .on_click(
                                cx.listener(move |page, _, _, cx| page.remove(id.clone(), cx)),
                            ),
                    );
            }
        }
        actions.into_any_element()
    }

    fn render_list(&self, cx: &mut Context<Self>) -> AnyElement {
        if !self.loaded {
            return div().into_any_element();
        }
        if self.rows.is_empty() {
            return div()
                .flex()
                .flex_col()
                .items_start()
                .gap(px(16.))
                .pt(px(12.))
                .child(
                    div()
                        .font_family(theme::DISPLAY_FONT)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_size(px(theme::SECTION))
                        .child("No downloads yet"),
                )
                .child(
                    div()
                        .max_w(px(520.))
                        .line_height(relative(1.5))
                        .text_color(theme::text_dim())
                        .child(
                            "Download episodes from a show's page to watch them offline. \
                             They show up here while they download.",
                        ),
                )
                .child(
                    button("downloads-search", "Find a show")
                        .kind(ButtonKind::Secondary)
                        .on_click(cx.listener(|page, _, _, cx| {
                            page.root
                                .update(cx, |root, cx| root.navigate(Route::Search, cx))
                                .ok();
                        })),
                )
                .into_any_element();
        }
        let mut list = div().flex().flex_col().gap(px(36.));
        for (title, statuses) in SECTIONS {
            let rows: Vec<(usize, &Row)> = self
                .rows
                .iter()
                .enumerate()
                .filter(|(_, r)| statuses.contains(&r.record.status))
                .collect();
            if rows.is_empty() {
                continue;
            }
            list = list.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap(px(12.))
                            .child(
                                div()
                                    .font_family(theme::DISPLAY_FONT)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_size(px(theme::SECTION))
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_size(px(theme::SMALL))
                                    .text_color(theme::text_muted())
                                    .child(rows.len().to_string()),
                            ),
                    )
                    .children(rows.into_iter().map(|(i, row)| self.render_row(i, row, cx))),
            );
        }
        list.into_any_element()
    }
}

impl Render for DownloadsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dir = cx
            .try_global::<AppState>()
            .map(|s| s.settings.download_dir.clone());
        div()
            .id("downloads")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(28.))
                    .px(theme::gutter())
                    .pt(px(48.))
                    .pb(px(64.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(10.))
                            .child(
                                div()
                                    .font_family(theme::DISPLAY_FONT)
                                    .font_weight(FontWeight::BOLD)
                                    .text_size(px(theme::PAGE_TITLE))
                                    .line_height(px(theme::PAGE_TITLE))
                                    .child("Downloads"),
                            )
                            .children(dir.map(|dir| {
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(10.))
                                    .min_w_0()
                                    .child(
                                        div()
                                            .min_w_0()
                                            .text_color(theme::text_dim())
                                            .truncate()
                                            .child(format!("Saving to {}", display_path(&dir))),
                                    )
                                    .child(
                                        button("downloads-open-folder", "Open folder")
                                            .kind(ButtonKind::Ghost)
                                            .compact()
                                            .on_click(move |_, _, cx| open_folder(dir.clone(), cx)),
                                    )
                            })),
                    )
                    .child(self.render_list(cx)),
            )
    }
}

/// What a row says under its title: progress, the file, or what went wrong.
fn status_line(row: &Row) -> AnyElement {
    let record = &row.record;
    let small = |text: String| {
        div()
            .text_size(px(theme::SMALL))
            .text_color(theme::text_muted())
            .truncate()
            .child(text)
    };
    match record.status {
        DlStatus::Queued => small("Waiting to start".into()).into_any_element(),
        DlStatus::Running => {
            let live = row.live.clone().unwrap_or_default();
            let percent = live
                .percent
                .or((record.progress > 0.0).then_some(record.progress));
            let mut parts: Vec<String> = Vec::new();
            match (percent, &live.downloaded) {
                (Some(p), _) => parts.push(format!("{p:.0}%")),
                (None, Some(bytes)) => parts.push(format!("{bytes} so far")),
                (None, None) => parts.push("Starting…".into()),
            }
            parts.extend(live.speed.clone());
            parts.extend(live.eta.as_ref().map(|eta| format!("{eta} left")));
            div()
                .flex()
                .items_center()
                .gap(px(14.))
                .child(
                    div()
                        .w(px(240.))
                        .flex_none()
                        .child(progress_bar(percent.unwrap_or(0.0) / 100.0)),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(12.))
                        .text_size(px(theme::SMALL))
                        .text_color(theme::text_dim())
                        .children(parts),
                )
                .into_any_element()
        }
        DlStatus::Done if !row.file_exists => div()
            .text_size(px(theme::SMALL))
            .text_color(theme::danger())
            .truncate()
            .child("The file was moved or deleted")
            .into_any_element(),
        DlStatus::Done => small(
            record
                .path
                .as_deref()
                .and_then(|p| Path::new(p).file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        )
        .into_any_element(),
        DlStatus::Failed => div()
            .text_size(px(theme::SMALL))
            .text_color(theme::danger())
            .truncate()
            .child(match &record.error {
                Some(err) => format!("Failed: {err}"),
                None => "Failed".into(),
            })
            .into_any_element(),
        DlStatus::Cancelled => small("Cancelled".into()).into_any_element(),
    }
}

fn quality_label(quality: &str) -> SharedString {
    match quality {
        "best" | "" => "Best quality".into(),
        q if q.chars().all(|c| c.is_ascii_digit()) => format!("{q}p").into(),
        q => q.to_string().into(),
    }
}

/// The manager's downloads as rows, and covers for shows not in `known`.
/// Touches the disk: call it off the UI thread.
fn snapshot(
    db: &Mutex<Db>,
    manager: &DownloadManager,
    known: &HashSet<String>,
) -> (Vec<Row>, HashMap<String, Option<String>>) {
    let rows: Vec<Row> = manager
        .snapshot()
        .into_iter()
        .map(|(record, live)| {
            let file_exists = record.status == DlStatus::Done
                && record
                    .path
                    .as_deref()
                    .is_some_and(|p| Path::new(p).is_file());
            Row {
                record,
                live,
                file_exists,
            }
        })
        .collect();
    let mut covers = HashMap::new();
    for row in &rows {
        let id = &row.record.show_id;
        if known.contains(id) || covers.contains_key(id) {
            continue;
        }
        let show = lock(db).get_show(id).ok().flatten();
        let cover = show.map(|s| {
            s.media
                .map(|m| m.cover_url)
                .filter(|c| !c.is_empty())
                .unwrap_or(s.summary.cover_url)
        });
        covers.insert(id.clone(), cover.filter(|c| !c.is_empty()));
    }
    (rows, covers)
}

/// The library's record of the download's show.
fn show_for(db: &Mutex<Db>, record: &DownloadRecord) -> Result<Option<ShowSummary>> {
    Ok(lock(db).get_show(&record.show_id)?.map(|s| s.summary))
}

/// Retries `record` under its id; one from an earlier session (which the
/// manager can't rerun) starts over as a new download replacing it.
fn retry_or_restart(
    db: &Mutex<Db>,
    manager: &DownloadManager,
    record: &DownloadRecord,
    dir: &Path,
) -> Result<()> {
    let Err(err) = manager.retry(&record.id) else {
        return Ok(());
    };
    let stopped = manager.snapshot().iter().any(|(r, _)| {
        r.id == record.id && matches!(r.status, DlStatus::Failed | DlStatus::Cancelled)
    });
    if !stopped {
        // Gone, or already running again.
        return Err(err);
    }
    let show =
        show_for(db, record)?.ok_or_else(|| anyhow!("start it again from the show's page"))?;
    playback::start_download(
        db,
        manager,
        &show,
        &record.ep,
        record.mode,
        &record.quality,
        dir,
    )?;
    manager.remove(&record.id);
    Ok(())
}

/// A request to play `record`'s file from where it was left, with the
/// show's other downloaded episodes (in this mode) to continue through.
fn play_request(
    db: &Mutex<Db>,
    manager: &Arc<DownloadManager>,
    record: &DownloadRecord,
) -> Result<PlayRequest> {
    let path = record
        .path
        .as_deref()
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .ok_or_else(|| anyhow!("the file was moved or deleted"))?;
    let show = show_for(db, record)?.unwrap_or_else(|| ShowSummary {
        id: record.show_id.clone(),
        title: record.title.clone(),
        ..ShowSummary::default()
    });
    let saved = lock(db).progress_for_show(&record.show_id, record.mode)?;
    let mut episodes: Vec<String> = manager
        .snapshot()
        .into_iter()
        .map(|(r, _)| r)
        .filter(|r| r.show_id == record.show_id && r.mode == record.mode)
        .filter(|r| r.status == DlStatus::Done)
        .map(|r| r.ep)
        .collect();
    episodes.sort_by(|a, b| episode_key(a).total_cmp(&episode_key(b)));
    episodes.dedup();
    Ok(PlayRequest {
        show,
        mode: record.mode,
        ep: record.ep.clone(),
        start: playback::resume_point(&saved, &record.ep),
        episodes,
        quality: record.quality.clone(),
        local_file: Some(path),
        stream: None,
    })
}

/// Numeric order for episode labels; non-numbers go last.
fn episode_key(ep: &str) -> f64 {
    ep.parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
        .unwrap_or(f64::INFINITY)
}
