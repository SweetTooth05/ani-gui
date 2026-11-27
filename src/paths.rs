use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const APP_DIR: &str = "ani-gui";

/// Where data, cache and downloads go when no `XDG_*` variable says
/// otherwise, relative to the home directory: the XDG defaults on Linux, the
/// standard Library folders on macOS. On Windows `%APPDATA%` / `%LOCALAPPDATA%`
/// are used before falling back to home. Explicit `XDG_*` variables win on
/// every platform.
const DATA_FALLBACK: &str = if cfg!(target_os = "macos") {
    "Library/Application Support"
} else {
    ".local/share"
};
const CACHE_FALLBACK: &str = if cfg!(target_os = "macos") {
    "Library/Caches"
} else {
    ".cache"
};
const VIDEOS_DIR: &str = if cfg!(target_os = "macos") {
    "Movies"
} else {
    "Videos"
};

fn home() -> PathBuf {
    env::home_dir().unwrap_or_default()
}

/// Returns `value` as a `PathBuf` when it is an absolute path (relative values
/// are invalid per the XDG spec).
fn abs_dir(value: Option<OsString>) -> Option<PathBuf> {
    value.map(PathBuf::from).filter(|dir| dir.is_absolute())
}

fn resolve_data_dir(xdg: Option<OsString>, appdata: Option<OsString>, home: &Path) -> PathBuf {
    if let Some(dir) = abs_dir(xdg) {
        return dir.join(APP_DIR);
    }
    if cfg!(target_os = "windows") {
        return abs_dir(appdata)
            .unwrap_or_else(|| home.to_path_buf())
            .join(APP_DIR);
    }
    home.join(DATA_FALLBACK).join(APP_DIR)
}

fn resolve_cache_dir(
    xdg: Option<OsString>,
    local_appdata: Option<OsString>,
    home: &Path,
) -> PathBuf {
    if let Some(dir) = abs_dir(xdg) {
        return dir.join(APP_DIR);
    }
    if cfg!(target_os = "windows") {
        return abs_dir(local_appdata)
            .unwrap_or_else(|| home.to_path_buf())
            .join(APP_DIR)
            .join("cache");
    }
    home.join(CACHE_FALLBACK).join(APP_DIR)
}

/// The folder above the one holding the running binary: `Contents` in
/// `ani-gui.app/Contents/MacOS/ani-gui`, else the install prefix (`/usr` for
/// the .deb, `usr` inside the AppImage, `~/.local` for `make install`,
/// `%LOCALAPPDATA%\Programs\ani-gui` on Windows).
/// Packaged builds keep ani-cli and libmpv under it.
pub fn exe_prefix() -> Option<PathBuf> {
    let exe = env::current_exe().ok()?;
    Some(exe.parent()?.parent()?.to_path_buf())
}

/// Where a packaged build keeps the ani-cli it ships, under [`exe_prefix`].
pub fn bundled_ani_cli(prefix: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        prefix.join("Resources/bin/ani-cli")
    } else {
        prefix.join("lib/ani-gui/bin/ani-cli")
    }
}

/// Where a Windows packaged build keeps busybox-w32, under [`exe_prefix`].
#[cfg(windows)]
pub fn bundled_busybox(prefix: &Path) -> PathBuf {
    prefix.join("lib/ani-gui/bin/busybox.exe")
}

pub fn data_dir() -> PathBuf {
    resolve_data_dir(
        env::var_os("XDG_DATA_HOME"),
        env::var_os("APPDATA"),
        &home(),
    )
}

pub fn cache_dir() -> PathBuf {
    resolve_cache_dir(
        env::var_os("XDG_CACHE_HOME"),
        env::var_os("LOCALAPPDATA"),
        &home(),
    )
}

pub fn db_path() -> PathBuf {
    data_dir().join("ani-gui.db")
}

pub fn default_download_dir() -> PathBuf {
    home().join(VIDEOS_DIR).join(APP_DIR)
}

pub fn ani_cli_history_path() -> PathBuf {
    // busybox-w32 and Git Bash honour an explicit HOME if set, else USERPROFILE.
    let ani_home = if cfg!(target_os = "windows") {
        env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(home)
    } else {
        home()
    };
    history_path(
        env::var_os("ANI_CLI_HIST_DIR"),
        env::var_os("XDG_STATE_HOME"),
        &ani_home,
    )
}

/// Mirrors ani-cli's
/// `${ANI_CLI_HIST_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/ani-cli}/ani-hsts`
/// exactly (empty counts as unset), so both programs share one file. ani-cli
/// uses the same location on macOS and Windows.
fn history_path(hist_dir: Option<OsString>, state_home: Option<OsString>, home: &Path) -> PathBuf {
    let non_empty = |v: Option<OsString>| v.filter(|v| !v.is_empty()).map(PathBuf::from);
    let dir = non_empty(hist_dir).unwrap_or_else(|| {
        non_empty(state_home)
            .unwrap_or_else(|| home.join(".local/state"))
            .join("ani-cli")
    });
    dir.join("ani-hsts")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_override_honors_absolute_and_ignores_relative() {
        #[cfg(windows)]
        let (home, abs) = (Path::new(r"C:\Users\u"), r"D:\x\data");
        #[cfg(not(windows))]
        let (home, abs) = (Path::new("/home/u"), "/x/data");

        assert_eq!(
            resolve_data_dir(Some(abs.into()), None, home),
            PathBuf::from(abs).join(APP_DIR)
        );
        assert_eq!(
            resolve_data_dir(Some("rel".into()), None, home),
            resolve_data_dir(None, None, home)
        );
    }

    #[test]
    fn fallbacks_match_the_platform() {
        #[cfg(windows)]
        {
            let home = Path::new(r"C:\Users\u");
            let appdata = Path::new(r"C:\Users\u\AppData\Roaming");
            let local = Path::new(r"C:\Users\u\AppData\Local");
            assert_eq!(
                resolve_data_dir(None, Some(appdata.into()), home),
                appdata.join("ani-gui")
            );
            assert_eq!(resolve_data_dir(None, None, home), home.join("ani-gui"));
            assert_eq!(
                resolve_cache_dir(None, Some(local.into()), home),
                local.join("ani-gui").join("cache")
            );
            assert_eq!(
                resolve_cache_dir(None, None, home),
                home.join("ani-gui").join("cache")
            );
            assert_eq!(
                resolve_cache_dir(Some(r"D:\x\cache".into()), Some(local.into()), home),
                PathBuf::from(r"D:\x\cache\ani-gui")
            );
        }
        #[cfg(not(windows))]
        {
            let home = Path::new("/Users/u");
            let (data, cache) = if cfg!(target_os = "macos") {
                (
                    "/Users/u/Library/Application Support/ani-gui",
                    "/Users/u/Library/Caches/ani-gui",
                )
            } else {
                ("/Users/u/.local/share/ani-gui", "/Users/u/.cache/ani-gui")
            };
            assert_eq!(resolve_data_dir(None, None, home), PathBuf::from(data));
            assert_eq!(resolve_cache_dir(None, None, home), PathBuf::from(cache));
            // An explicit XDG variable wins on every platform.
            assert_eq!(
                resolve_cache_dir(Some("/x/cache".into()), None, home),
                PathBuf::from("/x/cache/ani-gui")
            );
        }
    }

    #[test]
    fn history_path_env_order() {
        let home = Path::new("/home/u");
        assert_eq!(
            history_path(Some("/h".into()), Some("/s".into()), home),
            PathBuf::from("/h/ani-hsts")
        );
        assert_eq!(
            history_path(None, Some("/s".into()), home),
            PathBuf::from("/s/ani-cli/ani-hsts")
        );
        assert_eq!(
            history_path(Some("".into()), Some("".into()), home),
            PathBuf::from("/home/u/.local/state/ani-cli/ani-hsts")
        );
    }
}
