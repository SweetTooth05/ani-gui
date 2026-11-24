//! File-system helpers shared by pages: paths for display, and opening a
//! folder in the file manager.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::bail;
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

/// Opens `dir` (created first if missing) in the file manager.
pub fn open_folder(dir: PathBuf, cx: &mut App) {
    let opened = cx.background_spawn(async move {
        std::fs::create_dir_all(&dir)?;
        let status = Command::new("xdg-open").arg(&dir).status()?;
        if !status.success() {
            bail!("xdg-open exited with {status}");
        }
        anyhow::Ok(())
    });
    cx.spawn(async move |cx| {
        if let Err(err) = opened.await {
            cx.update(|cx| toast::error(cx, format!("Couldn't open the folder: {err:#}")))
                .ok();
        }
    })
    .detach();
}
