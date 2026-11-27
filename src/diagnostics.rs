//! Checks for the programs and libraries ani-gui depends on, for Settings.

use std::env;
use std::ffi::OsStr;
use std::path::PathBuf;

use crate::{anicli, player};

/// What to install when libmpv won't load, for Settings and the player.
pub const LIBMPV_INSTALL_HINT: &str = if cfg!(target_os = "macos") {
    "Install mpv (brew install mpv)"
} else if cfg!(target_os = "windows") {
    "Place libmpv-2.dll next to ani-gui.exe or on PATH"
} else {
    "Install libmpv2 (sudo apt install libmpv2)"
};

/// A sentence to append to a "not found" line telling how to install
/// `package`: Homebrew's command on macOS. Empty on Linux and Windows, where
/// tools are installed differently or bundled.
pub fn install_tip(package: &str) -> String {
    if cfg!(target_os = "macos") {
        format!(" With Homebrew: brew install {package}.")
    } else {
        String::new()
    }
}

pub struct Report {
    /// ani-cli's path and `-V` output.
    pub ani_cli: Option<(PathBuf, Option<String>)>,
    pub libmpv: Result<(), String>,
    pub ytdlp: bool,
    pub ffmpeg: bool,
    pub curl: bool,
}

/// Runs every check. Starts processes and loads libmpv: call it off the UI
/// thread. ani-cli's tools are looked up on the PATH ani-cli runs with.
pub fn run() -> Report {
    let path_var = anicli::tool_path();
    Report {
        ani_cli: anicli::find().map(|path| {
            let version = anicli::version(&path);
            (path, version)
        }),
        libmpv: player::probe_library(),
        ytdlp: which_in(&path_var, "yt-dlp"),
        ffmpeg: which_in(&path_var, "ffmpeg"),
        curl: which_in(&path_var, "curl"),
    }
}

/// Whether an executable `name` is in one of `path_var`'s absolute directories.
fn which_in(path_var: &OsStr, name: &str) -> bool {
    #[cfg(unix)]
    {
        env::split_paths(path_var)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(name))
            .any(|p| anicli::is_executable(&p))
    }
    #[cfg(not(unix))]
    {
        let candidates = [
            format!("{name}.exe"),
            format!("{name}.cmd"),
            format!("{name}.bat"),
            name.to_string(),
        ];
        env::split_paths(path_var)
            .filter(|dir| dir.is_absolute())
            .any(|dir| {
                candidates
                    .iter()
                    .map(|c| dir.join(c))
                    .any(|p| anicli::is_executable(&p))
            })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn diagnostics_reports_missing_binary() {
        assert!(!which_in(OsStr::new(""), "ffmpeg"));

        let dir = env::temp_dir().join(format!("ani-gui-which-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let tool = dir.join("tool");
            fs::write(&tool, "#!/bin/sh\n").unwrap();
            fs::set_permissions(&tool, fs::Permissions::from_mode(0o644)).unwrap();
            let path_var = env::join_paths([PathBuf::from("/nonexistent"), dir.clone()]).unwrap();
            assert!(!which_in(&path_var, "tool"), "not executable");
            fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
            assert!(which_in(&path_var, "tool"));
            assert!(!which_in(&path_var, "missing"));
        }
        #[cfg(not(unix))]
        {
            let tool = dir.join("tool.exe");
            fs::write(&tool, b"").unwrap();
            let path_var =
                env::join_paths([PathBuf::from(r"C:\nonexistent"), dir.clone()]).unwrap();
            assert!(which_in(&path_var, "tool"));
            assert!(!which_in(&path_var, "missing"));
        }
        // Relative entries are ignored, like an empty PATH.
        assert!(!which_in(OsStr::new("."), "tool"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn install_hints_match_the_platform() {
        if cfg!(target_os = "macos") {
            assert!(LIBMPV_INSTALL_HINT.contains("brew install mpv"));
            assert_eq!(
                install_tip("yt-dlp"),
                " With Homebrew: brew install yt-dlp."
            );
        } else if cfg!(target_os = "windows") {
            assert!(LIBMPV_INSTALL_HINT.contains("libmpv-2.dll"));
            assert_eq!(install_tip("yt-dlp"), "");
        } else {
            assert!(LIBMPV_INSTALL_HINT.contains("apt install libmpv2"));
            assert_eq!(install_tip("yt-dlp"), "");
        }
    }
}
