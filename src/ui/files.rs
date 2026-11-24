//! File-system helpers shared by pages: paths for display, and opening a
//! folder in the file manager.

use std::path::{Path, PathBuf};

use anyhow::anyhow;
use gpui::{App, AppContext};

use crate::opener;
use crate::ui::components::toast;

/// `path` with the home directory shown as "~".
pub fn display_path(path: &Path) -> String {
    match std::env::home_dir().and_then(|home| path.strip_prefix(home).ok().map(PathBuf::from)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Opens `dir` (created first if missing) in the file manager (see
/// [`opener::open`]).
pub fn open_folder(dir: PathBuf, cx: &mut App) {
    let started = cx.background_spawn(async move {
        std::fs::create_dir_all(&dir)?;
        anyhow::Ok(opener::open(dir.as_os_str())?)
    });
    cx.spawn(async move |cx| {
        let result = match started.await {
            Ok(rx) => match rx.await {
                Ok(Ok(status)) if !status.success() => {
                    Err(anyhow!("{} exited with {status}", opener::PROGRAM))
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
