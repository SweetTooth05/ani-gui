//! File-system helpers shared by pages: paths for display, and opening a
//! folder in the file manager.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

use anyhow::anyhow;
use futures::channel::oneshot;
use gpui::{App, AppContext};

use crate::ui::components::toast;

/// `path` with the home directory shown as "~".
pub fn display_path(path: &Path) -> String {
    match std::env::home_dir().and_then(|home| path.strip_prefix(home).ok().map(PathBuf::from)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Opens `dir` (created first if missing) in the file manager. xdg-open
/// may only exit with the file manager, so a thread of its own waits for it.
pub fn open_folder(dir: PathBuf, cx: &mut App) {
    let (tx, rx) = oneshot::channel();
    let started = cx.background_spawn(async move {
        std::fs::create_dir_all(&dir)?;
        let mut child = Command::new("xdg-open").arg(&dir).spawn()?;
        thread::Builder::new()
            .name("xdg-open".into())
            .spawn(move || {
                let _ = tx.send(child.wait());
            })?;
        anyhow::Ok(())
    });
    cx.spawn(async move |cx| {
        let result = match started.await {
            Ok(()) => match rx.await {
                Ok(Ok(status)) if !status.success() => {
                    Err(anyhow!("xdg-open exited with {status}"))
                }
                Ok(Err(err)) => Err(err.into()),
                _ => Ok(()),
            },
            Err(err) => Err(err),
        };
        if let Err(err) = result {
            cx.update(|cx| toast::error(cx, format!("Couldn't open the folder: {err:#}")))
                .ok();
        }
    })
    .detach();
}
