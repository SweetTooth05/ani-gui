# ani-gui native app (Rust + GPUI) — design

Date: 2026-11-23 · Status: approved by owner ("yes please implement now")

## Intent

Replace the Python local-web-server UI with a native Linux desktop application
written entirely in Rust with GPUI. It installs into the app launcher, opens
its own window, starts fast, and feels like a modern media client in the style
of Silo (poster-led, dark, hero banner, Continue Watching row, rich show page,
in-window player). ani-cli remains the stream-extraction engine.

This repo is the owner's fork (`SweetTooth05/ani-gui`); the Python app, PyPI,
Homebrew and macOS packaging belong to upstream and are removed here.

## Goals

- Native GPUI window, `.desktop` entry + icon, `make install` to `~/.local`.
- Video plays **inside the app window** (libmpv software render → GPUI image).
- Silo-like UI: sidebar nav, hero banner, Continue Watching, For You, Trending,
  show detail page, calendar, downloads, settings.
- SQLite for metadata cache, watch progress, My List, downloads, settings.
- Stays in sync with ani-cli's history file (`ani-cli -c` keeps working).

## Non-goals

macOS/Windows; accounts/multi-device sync; casting; ani-skip integration;
a plugin system; theming options (dark only); in-app ani-cli installer.

## Anti-goals

- Slow cold start (no blocking network on the UI thread; render from SQLite
  cache first, refresh in background).
- Breaking when ani-cli updates its extraction (we never re-implement
  extraction; we only consume what ani-cli hands a player).
- Leaking GPU memory per video frame.

## Approaches considered (playback)

1. **libmpv software render into GPUI frames — chosen.** Proven by Flick, Zym,
   macast-gpui. Keeps mpv's decoding, HLS, referer, external sub/audio tracks.
   Cost: CPU copy per frame (fine for 1080p).
2. GStreamer via `gpui-video-player`: weaker with ani-cli's mpv-tuned streams,
   pins GPUI versions.
3. Embed mpv's X11 window (`--wid`): forces XWayland on a Wayland session. Dropped.
(GPU texture sharing was excluded: GPUI has no external-texture API.)

## Decided "how"

### Stack
- Single crate at repo root, `gpui = "0.2.2"` from crates.io.
  - **Known build issue:** gpui 0.2.2 → `gpui_http_client` → `zed-async-tar` →
    `xattr 0.2.3`, which no longer compiles against current `libc` (`ENOATTR`
    removed). Fix: vendor `xattr 0.2.3` under `vendor/xattr`, replace
    `::libc::ENOATTR` with `::libc::ENODATA` (identical on Linux), and
    `[patch.crates-io] xattr = { path = "vendor/xattr" }`. Verified working.
  - System build deps (Ubuntu): `libxkbcommon-x11-dev libxkbcommon-dev
    libwayland-dev libvulkan-dev libfontconfig-dev libssl-dev libxcb1-dev
    libzstd-dev`. Runtime: `libmpv2`, `ani-cli`, `curl` (ani-cli's), plus
    `yt-dlp`/`ffmpeg` for downloads.
- No gpui-component (it tracks a different gpui); write our own small
  components. Text input: adapt gpui's own `examples/input.rs` (in the gpui
  0.2.2 crate source under `~/.cargo/registry/src/*/gpui-0.2.2/examples/`).
- HTTP: `ureq` (blocking) run on `cx.background_spawn`. JSON: `serde`/`serde_json`.
- SQLite: `rusqlite` with `bundled` feature. Migrations via `PRAGMA user_version`.
- libmpv: own minimal FFI loaded with `libloading` (`libmpv.so.2`) — no build-time
  link against libmpv; a missing library shows an in-app error instead of a
  crash.
- Paths via XDG: data `~/.local/share/ani-gui/ani-gui.db`, cache
  `~/.cache/ani-gui/images/`, downloads default `~/Videos/ani-gui`.

### Data sources
- **AllAnime** (as the Python app does today; constants in old `ani_gui/server.py`):
  search (`shows`) and episode list (`show.availableEpisodesDetail`). Request
  `aniListId`, `malId`, `englishName` too — AllAnime returns `aniListId`
  directly (verified), which links each show to AniList.
- **AniList GraphQL** (`https://graphql.anilist.co`): per-show detail (banner,
  extraLarge cover, description, genres, averageScore, seasonYear, format,
  status, nextAiringEpisode, streamingEpisodes thumbnails), Trending
  (`sort: TRENDING_DESC`), airing schedule for Calendar, recommendations for
  For You. Responses cached in SQLite (`api_cache`, TTL ~6 h).
- AniList item → playable AllAnime show: search AllAnime with the romaji title,
  pick the edge whose `aniListId` matches (fallback: first result).

### Stream resolution (verified 2026-11-23, revised)
ani-cli 5.1.8 searches **hianime.at**, not AllAnime, so result positions and ids
differ from our AllAnime catalog. We therefore never compute `-S` ourselves:
ani-cli's own "Select anime" menu is answered by our menu shim, and ani-cli's
own history write keeps `ani-cli -c` working with its ids.

Shims live in `<cache>/shims/`: `capture-mpv` (player; path must contain
"mpv"; `#!/bin/sh` + `for a in "$@"; do printf '%s\n' "$a"; done`),
`menu` (`#!/bin/sh` + `exec "<abs path of ani-gui exe>" --ani-cli-menu "$@"`),
and symlinks `rofi` and `dmenu` → `menu` (ani-cli forces rofi/dmenu when stdin
is not a tty, so the shim dir must come first on PATH).

```
PATH=<cache>/shims:/usr/local/bin:~/.local/bin:$PATH
ANI_CLI_PLAYER=<cache>/shims/capture-mpv  ANI_CLI_MENU=<cache>/shims/menu
ANI_GUI_WANT_TITLES="<title1>\n<title2>…"  ANI_GUI_MENU_OUT=<tmp file>
  ani-cli --no-detach [-S <n>] -e <ep> -q <quality> [--dub] "<query>"   </dev/null
```
- `query` = remembered provider title for this show, else AllAnime English
  title, else romaji title. `-S` is passed only after the user picked from a
  candidate list in the same session.
- Menu mode (`ani-gui --ani-cli-menu <args…>`, handled in `main` before GPUI
  starts): prompt = last argv item; stdin = `N title` lines.
  - Prompt contains "Select anime": pick the first line whose normalized title
    (lowercase, `’`→`'`, punctuation stripped, whitespace collapsed) equals a
    normalized want-title; print that line verbatim. No match → write the
    candidate titles (ani-cli order) as JSON array to `ANI_GUI_MENU_OUT`,
    print nothing, exit 0 (ani-cli then dies "Invalid anime selection").
  - Any other prompt (post-play "Playing episode…" menu): print nothing → ani-cli
    exits 0.
- stdout of the run yields `--referrer=…`, optional `--audio-file=…`, optional
  `--sub-file=…`, `--force-media-title=…`, final URL (verified).
- Outcomes: `Stream`; or `NeedsPick(candidates)` when the menu-out file is
  non-empty → UI shows "Which one is it?" with the candidates; choosing index i
  re-runs with `-S i` and stores the chosen title as the show's provider title
  (`provider_titles` table) so later plays match automatically; or failure
  (detail = last 240 chars of ANSI-stripped output). If ani-cli reports
  "No results found" for the English query, retry once with the romaji title.
- Timeout 60 s → kill process group.

### ani-cli history sync
ani-cli writes `ani-hsts` itself during the run above (verified: line
`3\tfrieren-beyond-journeys-end-481\tFrieren: Beyond Journey's End`). The app
never writes it. File: `$ANI_CLI_HIST_DIR` or `$XDG_STATE_HOME/ani-cli` or
`~/.local/state/ani-cli`, file `ani-hsts`, lines `ep\tid\ttitle` (ids are the
provider's, e.g. hianime slugs). On startup the app reads it for Continue
Watching: entries without app progress are matched to the catalog by title
(AllAnime search, normalized-title equality; no match → entry skipped, so a wrong show is never stored) and the
history title is stored as that show's provider title.

### Player
- `mpv_create`, options: `vo=libmpv`, `hwdec=auto-copy`, `referrer`,
  `force-media-title`, `keep-open=yes`; `loadfile` with `audio-file`/`sub-file`
  per-file options; `start=<secs>` for resume.
- Render: `mpv_render_context_create` with `MPV_RENDER_API_TYPE_SW`; a dedicated
  render thread woken by `mpv_render_context_set_update_callback` renders BGRA
  (`"bgr0"`) at the video view's pixel size into a buffer, publishes the
  latest frame, and notifies the GPUI view. The view wraps the frame in
  `Arc<RenderImage>` and paints it with `img()`; the **previous image must be
  dropped from the atlas** (`window.drop_image`) every frame. Consult Flick /
  macast-gpui for reference.
- Event thread: `mpv_observe_property` for `time-pos`, `duration`, `pause`,
  `eof-reached`, `paused-for-cache`; pushes state to the view.
- Controls overlay (auto-hides after 3 s idle): back, title + episode, play/pause,
  seek bar (elapsed / −remaining), −10 s / +10 s, +85 s "skip intro", next
  episode, volume, subtitle on/off, fullscreen. Keys: Space, F, Esc, ←/→,
  ↑/↓, N, M.
- Progress saved to SQLite every 5 s and on exit. ≥ 90 % watched = completed.
  Auto-play next episode at EOF (setting, default on) with a 5 s countdown card.

### Downloads
Run the same command as resolution but with `-d` instead of the player shim
(menu shim and want-titles still apply), with `ANI_CLI_DOWNLOAD_DIR` set, under a pseudo-terminal so yt-dlp/ffmpeg emit
progress; port `_parse_progress_line` / stall detection from server.py.
States: queued, running, done, failed, cancelled; cancel kills the process
group; retry re-issues. Completed files play in the in-app player
(`loadfile <path>`).

### SQLite schema (v1)
- `shows(id TEXT PK /*allanime*/, anilist_id INT, title, english_title,
  cover_url, banner_url, synopsis, genres_json, score INT, year INT, format,
  status, sub_count INT, dub_count INT, updated_at INT)`
- `progress(show_id, mode, ep TEXT, position REAL, duration REAL, updated_at INT,
  PRIMARY KEY(show_id, mode, ep))`
- `my_list(show_id TEXT PK, added_at INT)`
- `downloads(id TEXT PK, show_id, title, ep, mode, quality, path, status,
  progress REAL, error, created_at INT)`
- `provider_titles(show_id TEXT PK, title TEXT)` (schema v2)
- `settings(key TEXT PK, value TEXT)` — mode (sub/dub), quality, download_dir,
  autoplay_next.
- `api_cache(key TEXT PK, json TEXT, fetched_at INT)`
- Images: files in cache dir named by hash of URL (no table). Relative AllAnime
  cover paths need the CDN + `Referer` header (see server.py `COVER_CDN`).

### UI
Dark only; poster art supplies color; one accent. Left sidebar: Home, Search,
My List, Calendar, Downloads, Settings.
- **Home:** hero banner (first Continue Watching show, else top Trending):
  banner image with gradient, title, genres · year · score, synopsis, Resume /
  Play and + My List. Rows: Continue Watching (landscape cards, episode label,
  progress bar), For You, Trending now (portrait posters). Rows scroll
  horizontally.
- **Search:** text field (debounced 300 ms), Sub/Dub toggle, poster grid.
- **Show page:** banner + cover, metadata, synopsis, genres, Sub/Dub toggle,
  quality picker, Play/Resume, My List toggle, Download-range action, episode
  list (thumbnail where AniList has one, number, title, per-episode progress,
  Download button).
- **My List:** poster grid. **Calendar:** next 7 days of airing anime by day.
- **Downloads:** list with progress, speed, cancel/retry/play/remove.
- **Settings:** default mode, quality, download dir, autoplay next; Diagnostics
  section (ani-cli path + version, libmpv loaded?, yt-dlp/ffmpeg present).
- Errors are shown inline (toast/banner), never as panics.
- Use the `frontend-design` skill for visual direction.

### Packaging
`Makefile` (`build`, `install PREFIX=~/.local`, `uninstall`), `assets/ani-gui.desktop`
(app id `io.github.sweettooth05.AniGui`), scalable SVG icon in hicolor. README
rewritten for the Rust app. CI: `cargo build` + `cargo test` + `cargo clippy` on
Ubuntu with the system deps.

### Removal
Delete `ani_gui/`, `pyproject.toml`, `ani-gui` (Python launcher script),
`Formula/`, `packaging/macos/`, `DISTRIBUTION.md`, `docs/index.html`, old
`install.sh` (replace with a Linux installer that builds + `make install`).
Version 1.0.0; CHANGELOG entry.

## Left to the builder

Exact module boundaries, component styling details, font choice (bundling an
OFL font such as Inter / Plus Jakarta Sans is fine), retry/backoff policy,
cache eviction (simple size cap is fine).
