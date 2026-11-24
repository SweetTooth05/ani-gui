use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const APP_DIR: &str = "ani-gui";

/// Where data, cache and downloads go when no `XDG_*` variable says
/// otherwise, relative to the home directory: the XDG defaults on Linux, the
/// standard Library folders on macOS. Explicit `XDG_*` variables win on both.
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

/// Resolves an XDG base directory: the variable's value when it is an absolute
/// path (relative values are invalid per the XDG spec), else `home/fallback`.
fn xdg_base(value: Option<OsString>, home: &Path, fallback: &str) -> PathBuf {
    match value.map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir,
        _ => home.join(fallback),
    }
}

pub fn data_dir() -> PathBuf {
    xdg_base(env::var_os("XDG_DATA_HOME"), &home(), DATA_FALLBACK).join(APP_DIR)
}

pub fn cache_dir() -> PathBuf {
    xdg_base(env::var_os("XDG_CACHE_HOME"), &home(), CACHE_FALLBACK).join(APP_DIR)
}

pub fn db_path() -> PathBuf {
    data_dir().join("ani-gui.db")
}

pub fn default_download_dir() -> PathBuf {
    home().join(VIDEOS_DIR).join(APP_DIR)
}

pub fn ani_cli_history_path() -> PathBuf {
    history_path(
        env::var_os("ANI_CLI_HIST_DIR"),
        env::var_os("XDG_STATE_HOME"),
        &home(),
    )
}

/// Mirrors ani-cli's
/// `${ANI_CLI_HIST_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/ani-cli}/ani-hsts`
/// exactly (empty counts as unset), so both programs share one file. ani-cli
/// uses the same location on macOS.
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
    fn xdg_base_honors_absolute_and_ignores_relative() {
        let home = Path::new("/home/u");
        assert_eq!(
            xdg_base(Some("/x/data".into()), home, ".local/share"),
            PathBuf::from("/x/data")
        );
        assert_eq!(
            xdg_base(Some("rel".into()), home, ".local/share"),
            PathBuf::from("/home/u/.local/share")
        );
        assert_eq!(
            xdg_base(None, home, ".cache"),
            PathBuf::from("/home/u/.cache")
        );
    }

    #[test]
    fn fallbacks_match_the_platform() {
        let home = Path::new("/Users/u");
        let (data, cache) = if cfg!(target_os = "macos") {
            (
                "/Users/u/Library/Application Support",
                "/Users/u/Library/Caches",
            )
        } else {
            ("/Users/u/.local/share", "/Users/u/.cache")
        };
        assert_eq!(xdg_base(None, home, DATA_FALLBACK), PathBuf::from(data));
        assert_eq!(xdg_base(None, home, CACHE_FALLBACK), PathBuf::from(cache));
        // An explicit XDG variable wins on every platform.
        assert_eq!(
            xdg_base(Some("/x/cache".into()), home, CACHE_FALLBACK),
            PathBuf::from("/x/cache")
        );
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
