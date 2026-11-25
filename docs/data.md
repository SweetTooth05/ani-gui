# Where your data lives

| What | Where |
|---|---|
| Library, watch progress, My List, downloads list, settings, imported MyAnimeList/AniList lists (SQLite) | `~/.local/share/ani-gui/ani-gui.db` (`$XDG_DATA_HOME/ani-gui/`) |
| MyAnimeList / AniList sign-in tokens | Your system keychain (service `ani-gui`) |
| Cover art and the helper scripts ani-cli calls | `~/.cache/ani-gui/` (`$XDG_CACHE_HOME/ani-gui/`) |
| Downloaded episodes | `~/Videos/ani-gui` by default; change it in Settings |
| Watch history shared with ani-cli | `~/.local/state/ani-cli/ani-hsts` (ani-cli's own file; `$ANI_CLI_HIST_DIR` and `$XDG_STATE_HOME` are honoured) |

The paths above are for Linux. For macOS, see [Installing on macOS](install.md#macos). The cache is
safe to delete at any time.

