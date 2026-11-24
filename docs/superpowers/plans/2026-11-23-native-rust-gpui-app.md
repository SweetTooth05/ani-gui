# ani-gui Native App (Rust + GPUI) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the Python web UI with a native Linux GPUI app that browses anime (AllAnime + AniList), plays episodes inside its own window via libmpv, and stores metadata/progress in SQLite.

**Architecture:** One Rust crate at the repo root. Pure-logic modules (db, allanime, anilist, anicli, downloads, progress) are unit-tested without a window; `player` wraps libmpv (dlopen) with a software-render thread producing BGRA frames; `ui` is GPUI views over a shared `AppState` global. ani-cli is used only to resolve stream links (capture shim) and to download.

**Tech Stack:** Rust 1.98, gpui 0.2.2, rusqlite (bundled), ureq, serde/serde_json, libloading, nix (pty/signal), anyhow.

**Spec:** `docs/superpowers/specs/2026-11-23-native-rust-gpui-app-design.md` — read it before every task; it holds the verified ani-cli capture protocol, schema, API endpoints and UI layout.

## Global Constraints

- Linux only. Binary name `ani-gui`; app id `io.github.sweettooth05.AniGui`; version `1.0.0`.
- `gpui = "0.2.2"` with `[patch.crates-io] xattr = { path = "vendor/xattr" }` (vendored xattr 0.2.3, `::libc::ENOATTR` → `::libc::ENODATA`). A working copy exists at `/tmp/claude-1000/-home-kevin-ani-gui/f7edd555-7ae3-4edd-b53e-01c82e19bc46/scratchpad/spike/vendor/xattr` — copy it.
- No network or disk I/O on the GPUI main thread: use `cx.background_spawn` and update entities when done.
- libmpv is loaded at runtime with `libloading` from `libmpv.so.2`; never linked at build time.
- No `unwrap()`/`expect()` on fallible I/O, network, or parse paths outside tests; errors surface in the UI.
- XDG paths: DB `~/.local/share/ani-gui/ani-gui.db`; cache `~/.cache/ani-gui/` (images, `capture-mpv` shim); downloads default `~/Videos/ani-gui`. Honor `XDG_DATA_HOME`/`XDG_CACHE_HOME`.
- AllAnime constants (API URL, Referer `https://youtu-chan.com`, User-Agent, cover CDN) copied from `ani_gui/server.py` lines 37-43 (file is deleted only in Task 8).
- Every task ends green on: `cargo build && cargo test && cargo clippy --all-targets -- -D warnings`.
- Commit after each task with the attribution trailer `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **Show with 0 episodes in the chosen mode (e.g. Dub missing)** → Play/episode list shows "No dub episodes" and offers Sub; never calls ani-cli. Test in Task 2 (`search_filters_zero_count_mode`) and Task 5 (`show_page_model_no_episodes_in_mode`).
2. **ani-cli missing or resolve fails/times out (60 s)** → inline error with ani-cli's stderr detail, player view returns to the show page. Test in Task 3 (`parse_capture_rejects_missing_url`, `resolve_times_out`).
3. **libmpv.so.2 missing** → app still launches; Play shows "Install libmpv2" error; Settings→Diagnostics shows it. Test in Task 4 (`load_library_missing_returns_error`).
4. **Offline / AniList rate-limited (HTTP 429)** → home renders from `api_cache` even if stale; rows hidden if no cache; no panic. Test in Task 2 (`cache_get_stale_allowed`).
5. **ani-cli history file with malformed / blank lines or special chars in title (tabs excluded)** → skipped lines preserved on rewrite, file replaced atomically. Test in Task 3 (`record_history_preserves_other_lines`).

---

### Task 1: Crate scaffold, paths, SQLite store

**Files:**
- Create: `Cargo.toml`, `vendor/xattr/**`, `src/main.rs`, `src/paths.rs`, `src/model.rs`, `src/db.rs`, `.gitignore` (add `/target`)

**Interfaces:**
- Produces (`src/model.rs`):
  - `enum Mode { Sub, Dub }` with `as_str() -> &'static str` ("sub"/"dub"), `FromStr`, `Default = Sub`.
  - `struct ShowSummary { id: String, title: String, english_title: Option<String>, cover_url: String, anilist_id: Option<i64>, sub_count: u32, dub_count: u32 }` + `fn count(&self, Mode) -> u32`, `fn display_title(&self) -> &str` (english if present).
  - `struct AniListMedia { id: i64, title_romaji: String, title_english: Option<String>, cover_url: String, banner_url: Option<String>, synopsis: Option<String> /*HTML stripped*/, genres: Vec<String>, score: Option<u8>, year: Option<u16>, format: Option<String>, status: Option<String>, episodes: Option<u32>, next_airing: Option<(u32, i64)>, episode_thumbs: Vec<(String, String)> /*(title, url)*/ }`
  - `struct Airing { media: AniListMedia, episode: u32, airing_at: i64 }`
  - `struct Stream { url: String, referrer: Option<String>, audio_file: Option<String>, sub_file: Option<String>, title: Option<String> }`
  - `struct EpisodeRequest { query: String, nth: usize, ep: String, mode: Mode, quality: String }`
  - `struct Progress { show_id: String, mode: Mode, ep: String, position: f64, duration: f64, updated_at: i64 }`
  - `struct ShowRecord { summary: ShowSummary, media: Option<AniListMedia>, updated_at: i64 }`
  - `enum DlStatus { Queued, Running, Done, Failed, Cancelled }` (+ `as_str`/`FromStr`)
  - `struct DownloadRecord { id: String, show_id: String, title: String, ep: String, mode: Mode, quality: String, path: Option<String>, status: DlStatus, progress: f32, error: Option<String>, created_at: i64 }`
  - `struct Settings { mode: Mode, quality: String /*best*/, download_dir: PathBuf, autoplay_next: bool /*true*/ }`
- Produces (`src/paths.rs`): `fn data_dir() -> PathBuf`, `fn cache_dir() -> PathBuf`, `fn db_path() -> PathBuf`, `fn default_download_dir() -> PathBuf`, `fn ani_cli_history_path() -> PathBuf` (spec's env-var order).
- Produces (`src/db.rs`): `struct Db` with `open(path: &Path) -> anyhow::Result<Db>`, `open_in_memory()`, and:
  `upsert_show(&ShowRecord)`, `get_show(id) -> Option<ShowRecord>`,
  `save_progress(&Progress)`, `progress_for_show(show_id, Mode) -> Vec<Progress>`, `recent_progress(limit) -> Vec<Progress>` (latest row per show, newest first),
  `my_list_add(id)`, `my_list_remove(id)`, `my_list_contains(id) -> bool`, `my_list() -> Vec<String>` (newest first),
  `upsert_download(&DownloadRecord)`, `list_downloads() -> Vec<DownloadRecord>`, `delete_download(id)`,
  `load_settings() -> Settings`, `save_settings(&Settings)`,
  `cache_get(key, max_age_secs: i64) -> Option<String>`, `cache_get_stale(key) -> Option<String>`, `cache_put(key, json)`.
  `media` stored as JSON column `media_json` in `shows` (in addition to spec columns is fine).

- [ ] **Step 1: Write failing tests in `src/db.rs` `#[cfg(test)]`**: `migrate_sets_user_version_1`, `show_roundtrip`, `progress_upsert_replaces_same_key`, `recent_progress_latest_per_show` (two shows, three rows → 2 rows, newest first), `my_list_add_remove_order`, `settings_defaults_then_roundtrip` (defaults: Sub, "best", autoplay true), `cache_get_respects_age` (age 0 → None after put with max_age −1 semantics: insert with `fetched_at = now-100`, `cache_get(key, 50)` → None, `cache_get_stale` → Some), `download_roundtrip_and_delete`.
- [ ] **Step 2:** `cargo test` → fails (no impl).
- [ ] **Step 3:** Implement. `main.rs` opens a GPUI window titled "ani-gui" (dark background, placeholder text) and opens the DB at startup (error printed, app continues). Migrations: `PRAGMA user_version`; v1 creates all spec tables; enable WAL.
- [ ] **Step 4:** `cargo build && cargo test && cargo clippy --all-targets -- -D warnings` → pass. `timeout 5 cargo run` → exits 124 (window ran, no panic).
- [ ] **Step 5:** Commit `feat: rust crate scaffold with sqlite store`.

### Task 2: AllAnime + AniList clients, image cache, catalog caching

**Files:**
- Create: `src/net.rs`, `src/allanime.rs`, `src/anilist.rs`, `src/images.rs`, `src/catalog.rs`, `tests/fixtures/allanime_search.json`, `tests/fixtures/allanime_show.json`, `tests/fixtures/anilist_media.json`, `tests/fixtures/anilist_schedule.json` (captured once with curl from live APIs; trim to ≤3 items)

**Interfaces:**
- Consumes: `model::*`, `db::Db`.
- Produces:
  - `net::post_json(url, body: &Value, headers: &[(&str,&str)]) -> anyhow::Result<Value>`, `net::get_bytes(url, headers) -> anyhow::Result<Vec<u8>>` (ureq, 20 s timeout, User-Agent `ani-gui/1.0`, except AllAnime uses its own UA).
  - `allanime::parse_search(v: &Value, mode: Mode) -> Vec<ShowSummary>` (only shows with count>0 in mode, API order kept; request fields `_id name englishName thumbnail aniListId availableEpisodes`; relative thumbnail → `COVER_CDN/<path>`), `allanime::search(query, mode) -> Result<Vec<ShowSummary>>`, `allanime::parse_episodes(v, mode) -> Vec<String>` (numeric sort, non-numeric last), `allanime::episodes(show_id, mode) -> Result<Vec<String>>`, `allanime::nth_of(results: &[ShowSummary], show_id) -> Option<usize>` (1-based).
  - `anilist::parse_media(v: &Value) -> AniListMedia` (strip HTML tags/`<br>` from description; score = averageScore), `anilist::media(id) -> Result<AniListMedia>`, `anilist::trending(n) -> Result<Vec<AniListMedia>>`, `anilist::schedule(from_ts, to_ts) -> Result<Vec<Airing>>` (paginate up to 3 pages, `perPage: 50`), `anilist::recommendations(ids: &[i64]) -> Result<Vec<AniListMedia>>` (union of each media's `recommendations(sort: RATING_DESC)` top 10, de-duplicated, excluding `ids`).
  - `images::cache_key(url) -> String` (FNV-1a 64 hex + original extension), `images::cached_image(url, cache_dir) -> Result<PathBuf>` (download once; adds AllAnime Referer for the cover CDN host; writes via tmp+rename).
  - `catalog::{trending(db: &Mutex<Db>) -> Vec<AniListMedia>, schedule_week(db) -> Vec<Airing>, media(db, id) -> Option<AniListMedia>, for_you(db, seed_ids: &[i64]) -> Vec<AniListMedia>}` — each: fresh cache (6 h) → else fetch+put → on fetch error use `cache_get_stale` → else empty. `catalog::resolve_allanime(media: &AniListMedia, mode) -> Result<Option<ShowSummary>>` (search romaji title, pick matching `anilist_id`, else english title search, else first result).

- [ ] **Step 1: Failing tests** (fixture-based, no network): `parse_search_maps_fields` (aniListId "182255" → `Some(182255)`, dub_count 10), `search_filters_zero_count_mode` (fixture with a sub-only show; Dub parse excludes it), `parse_episodes_sorted_numeric` (`["10","2","1","5.5"]` → `["1","2","5.5","10"]`), `nth_of_is_one_based`, `parse_media_strips_html`, `parse_schedule_items`, `cache_key_stable_and_keeps_ext`, `cache_get_stale_allowed` (catalog falls back to stale json when the fetch closure errors — make the fetch injectable: `fn cached_or<F: FnOnce() -> Result<Value>>(db, key, ttl, fetch: F) -> Option<Value>`), `resolve_prefers_matching_anilist_id` (pure helper `pick_match(results, anilist_id) -> Option<ShowSummary>`).
- [ ] **Step 2:** `cargo test` → new tests fail.
- [ ] **Step 3:** Implement. Add one `#[ignore]` live test `live_search_frieren` for manual runs.
- [ ] **Step 4:** Full check command passes; `cargo test -- --ignored live_` passes with network.
- [ ] **Step 5:** Commit `feat: allanime/anilist clients, image cache, cached catalog`.

### Task 3: ani-cli integration — resolve, history, downloads, continue-watching

**Files:**
- Create: `src/anicli.rs`, `src/history.rs`, `src/downloads.rs`, `src/progress.rs`

**Interfaces:**
- Consumes: `model::*`, `db::Db`, `paths::*`, `allanime::{search, nth_of}`.
- Produces:
  - `anicli::find() -> Option<PathBuf>` (PATH + `~/.local/bin` + `/usr/local/bin`), `anicli::version(path) -> Option<String>` (`ani-cli -V`), `anicli::ensure_capture_shim(cache_dir) -> io::Result<PathBuf>` (file `capture-mpv`, mode 0755, content per spec), `anicli::parse_capture(stdout: &str) -> anyhow::Result<Stream>` (ignore ANSI/status lines; URL = last line starting with `http` that is not a `--` flag), `anicli::resolve(req: &EpisodeRequest) -> anyhow::Result<Stream>` (exact command/env from spec; 60 s timeout → kill; on failure include last 240 chars of ANSI-stripped output), `anicli::build_request(show: &ShowSummary, ep, mode, quality) -> anyhow::Result<EpisodeRequest>` (query = `show.title`, nth via live search + `nth_of`; error "show not found in ani-cli search").
  - `history::{HistoryEntry { ep, id, title }, read(path) -> Vec<HistoryEntry>, record(path, &HistoryEntry) -> io::Result<()>}` — `record` replaces the line whose id matches (else appends), keeps all other lines byte-for-byte, writes tmp+rename, creates parent dir.
  - `progress::{is_completed(p: &Progress) -> bool /* duration>0 && position/duration >= 0.9 */, continue_point(latest: &Progress, episodes: &[String]) -> Option<(String, f64)>, history_continue(entry_ep: &str, episodes: &[String]) -> Option<String>}` — completed → next ep at 0.0 (None if last); else same ep at position; history: next after entry ep.
  - `downloads::{parse_progress_line(line) -> Option<DlProgress { percent: Option<f32>, speed: Option<String>, eta: Option<String>, downloaded: Option<String> }>, DownloadManager}` — port `_parse_progress_line` (yt-dlp, aria2c, ffmpeg) from `ani_gui/server.py:187`. `DownloadManager::new(db: Arc<Mutex<Db>>) -> Self`, `start(&self, show: &ShowSummary, req: EpisodeRequest, dir: &Path) -> String /*id*/`, `cancel(id)`, `retry(id)`, `remove(id)`, `snapshot() -> Vec<(DownloadRecord, Option<DlProgress>)>`, `set_on_change(Box<dyn Fn() + Send + Sync>)`. Runs `ani-cli -d …` under a pty (`nix::pty::openpty`, own process group via `setsid`), splits output on `\r`/`\n`, stall after 60 s without percent change → Failed "stalled". Done when exit 0; path = newest file in dir matching `Episode <ep>` (port `_match_download_files`). Persist every status change.

- [ ] **Step 1: Failing tests:** `parse_capture_full` (input = the verified sample: `--referrer=https://zokoanime.video/`, `--sub-file=https://…vtt`, `--force-media-title=Frieren… Episode 1`, `https://hls…m3u8`, preceded by ANSI status lines) → all fields set, audio None; `parse_capture_rejects_missing_url`; `resolve_times_out` (point `ANI_CLI` override at a script that sleeps 5 s with timeout 1 s — make timeout a parameter of an internal `run_capture(cmd, env, timeout)`); `record_history_replaces_and_appends`; `record_history_preserves_other_lines` (blank + malformed lines survive); `continue_point_completed_moves_next`, `continue_point_partial_resumes`, `continue_point_last_episode_none`; `parse_progress_ytdlp` (`[download]  22.5% of ~123.45MiB at  2.34MiB/s ETA 00:42` → 22.5, "2.34MiB/s", "00:42"), `parse_progress_aria2c`, `parse_progress_ffmpeg`, `parse_progress_ignores_noise`.
- [ ] **Step 2:** `cargo test` → fail.
- [ ] **Step 3:** Implement. Add `#[ignore]` `live_resolve_frieren` (resolves `-S 2 -e 1 frieren` and asserts `url.starts_with("http")` and referrer present).
- [ ] **Step 4:** Full check passes; `cargo test -- --ignored live_resolve` passes.
- [ ] **Step 5:** Commit `feat: ani-cli resolve via capture shim, history sync, downloads`.

### Task 4: libmpv player engine (software render)

**Files:**
- Create: `src/player/mod.rs`, `src/player/ffi.rs`

**Interfaces:**
- Consumes: `model::Stream`.
- Produces:
  - `player::Frame { width: u32, height: u32, data: Vec<u8> /* BGRA, stride = width*4 */, serial: u64 }`
  - `player::PlaybackState { time_pos: f64, duration: f64, paused: bool, eof: bool, buffering: bool, volume: f64, sub_visible: bool }`
  - `player::PlayerError::{LibraryMissing(String), Mpv(String)}` (thiserror or manual Display)
  - `player::MpvPlayer::new() -> Result<MpvPlayer, PlayerError>`, `load(&self, s: &Stream, start_secs: f64)`, `toggle_pause()`, `set_pause(bool)`, `seek_relative(f64)`, `seek_absolute(f64)`, `set_volume(f64)`, `toggle_subs()`, `stop()`, `set_render_size(w: u32, h: u32)`, `latest_frame(&self) -> Option<Arc<Frame>>`, `state(&self) -> PlaybackState`, `set_on_change(&self, Box<dyn Fn() + Send + Sync>)`. `Drop` stops threads, frees render context before `mpv_terminate_destroy`.
  - Local file playback = `Stream { url: <path>, ..None }`.
- Approach: `ffi.rs` declares only the needed symbols (`mpv_create`, `mpv_initialize`, `mpv_set_option_string`, `mpv_set_property_string`, `mpv_command`, `mpv_observe_property`, `mpv_wait_event`, `mpv_wakeup`, `mpv_terminate_destroy`, `mpv_render_context_create`, `mpv_render_context_set_update_callback`, `mpv_render_context_update`, `mpv_render_context_render`, `mpv_render_context_free`, `mpv_error_string`) with constants from `mpv/client.h` and `mpv/render.h` (headers installed at `/usr/include/mpv/`). SW render params: `MPV_RENDER_PARAM_API_TYPE="sw"`, render with `SW_SIZE`, `SW_FORMAT="bgr0"`, `SW_STRIDE`, `SW_POINTER`. Render thread waits on a condvar signaled by the update callback; if `mpv_render_context_update` has `MPV_RENDER_UPDATE_FRAME`, render at current render size (default 1280×720, never 0) and swap the frame `Arc`. Event thread loops `mpv_wait_event` and updates `PlaybackState`. Options from spec (`vo=libmpv`, `hwdec=auto-copy`, `keep-open=yes`); per-file `referrer`, `audio-file`, `sub-file`, `force-media-title`, `start` passed as `loadfile <url> replace -1 <opts>` (mpv ≥0.38 syntax; libmpv here is 0.41).

- [ ] **Step 1: Failing tests:** `load_library_missing_returns_error` (internal `MpvPlayer::with_library(name)` given `"libdoes-not-exist.so"` → `LibraryMissing`); `plays_local_file_and_produces_frames` — generate `testsrc` clip with `ffmpeg -f lavfi -i testsrc=duration=3:size=320x240:rate=25 -y $TMP/t.mp4` (skip test with a printed note if ffmpeg or libmpv missing), load, wait ≤5 s: a frame with width>0 arrives, `state().duration` ≈ 3.0 ±0.2, `time_pos` advances; `seek_absolute(2.0)` then time_pos ≥ 1.9 within 2 s; `set_render_size(160,120)` → next frame is 160×120.
- [ ] **Step 2:** `cargo test player` → fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Full check passes.
- [ ] **Step 5:** Commit `feat: libmpv software-render player engine`.

### Task 5: App shell, design system, Home / Search / Show pages

**REQUIRED SKILL for implementer:** `frontend-design` (visual direction: Silo-like, dark, poster-led, one accent).

**Files:**
- Create: `src/app.rs` (AppState global + Router), `src/ui/mod.rs`, `src/ui/theme.rs`, `src/ui/components/{mod.rs,button.rs,poster.rs,row.rs,text_input.rs,image.rs,toast.rs}`, `src/ui/sidebar.rs`, `src/ui/home.rs`, `src/ui/search.rs`, `src/ui/show.rs`, `src/ui/view_models.rs`, `assets/fonts/*` (optional OFL font)
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: Tasks 1–3 modules.
- Produces:
  - `app::AppState` (gpui `Global`): `db: Arc<Mutex<Db>>`, `settings: Settings`, `downloads: Arc<DownloadManager>`, `cache_dir: PathBuf`.
  - `app::Route { Home, Search, MyList, Calendar, Downloads, Settings, Show(ShowRef), Player(PlayRequest) }`, `ShowRef { allanime: Option<ShowSummary>, anilist_id: Option<i64>, title: String }`, `PlayRequest { show: ShowSummary, mode: Mode, ep: String, start: f64, episodes: Vec<String>, local_file: Option<PathBuf> }`.
  - `app::Root` view with `fn navigate(&mut self, Route, cx)` and a back stack (`fn back`). Task 6 renders `Route::Player`; Task 7 renders MyList/Calendar/Downloads/Settings — leave those as a styled "coming soon" placeholder here.
  - `ui::components::image::remote_image(url: &str) -> impl IntoElement` — resolves via `images::cached_image` in background, shows a shimmer placeholder until ready (process-wide in-memory map url→path).
  - `ui::view_models::{continue_items(db, history) -> Vec<ContinueItem { show_id, title, ep, start, fraction, cover_url }>, hero_pick(continue: &[ContinueItem], trending: &[AniListMedia]) -> Option<HeroModel>, ShowPageModel::build(show, media, episodes, progress, mode) -> ShowPageModel { play_label /*"Play E1" | "Resume E5" | "No dub episodes"*/, play_target: Option<(String, f64)>, episodes: Vec<EpisodeRow { ep, title, thumb, fraction, completed }> }}`.
- Pages per spec "UI" section (Home hero + rows, Search with debounced input + Sub/Dub toggle + grid, Show page with Play/Resume, My List toggle, quality picker, episode list with per-episode Download that calls `DownloadManager::start`). Clicking Play navigates to `Route::Player(PlayRequest)`. Home loads from cache first, then refreshes in background.

- [ ] **Step 1: Failing tests** in `view_models.rs`: `hero_prefers_continue_watching`, `hero_falls_back_to_trending`, `show_page_model_resume_label` (progress ep 5 at 40% → "Resume E5", target ("5", pos)), `show_page_model_play_first` ("Play E1"), `show_page_model_no_episodes_in_mode` ("No dub episodes", target None), `continue_items_merges_history_without_progress` (history entry ep "3" with episodes 1..5 and no db progress → item ep "4", start 0).
- [ ] **Step 2:** `cargo test view_models` → fail.
- [ ] **Step 3:** Implement models, theme, components, pages.
- [ ] **Step 4:** Full check passes; `timeout 8 cargo run --release` exits 124 with no panic in stderr; the owner/controller visually checks Home, Search ("frieren"), and a Show page.
- [ ] **Step 5:** Commit `feat: gpui shell with home, search and show pages`.

### Task 6: Player view and playback flow

**REQUIRED SKILL for implementer:** `frontend-design` (player overlay).

**Files:**
- Create: `src/ui/player_view.rs`, `src/playback.rs`
- Modify: `src/app.rs` (render `Route::Player`), `src/ui/show.rs` (pass episodes)

**Interfaces:**
- Consumes: `player::MpvPlayer`, `anicli::{build_request, resolve}`, `history::record`, `db.save_progress`, `progress::*`, `AppState`.
- Produces:
  - `playback::next_episode(episodes: &[String], ep: &str) -> Option<String>`, `playback::should_save(last_saved_at: f64, now_pos: f64) -> bool` (every ≥5 s of movement), `playback::history_entry(show, ep) -> HistoryEntry` (title = `"<title> (<n> episodes)"` like ani-cli writes, n = sub/dub count).
  - `PlayerView::new(req: PlayRequest, window, cx)` — states: Resolving (spinner + "Finding stream…"), Playing, Error(msg + Back). One `MpvPlayer` per view; dropped on leaving. Frame painting: each new `Frame` → `Arc<RenderImage>` (BGRA, `image::Frame` from `RgbaImage` buffer as gpui expects), paint with `img()` object-fit contain on black; call `window.drop_image(prev)` when replacing. `set_render_size` from the element's bounds × scale factor (debounced on resize).
  - Overlay + keys + autoplay countdown exactly as spec "Player". On first `time_pos > 0`: `history::record`. Save progress on interval, on pause, on back, on EOF.

- [ ] **Step 1: Failing tests** in `playback.rs`: `next_episode_middle_and_last`, `should_save_every_5s`, `history_entry_title_format` (title "Frieren", 28 sub eps → "Frieren (28 episodes)").
- [ ] **Step 2:** `cargo test playback` → fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Full check passes. Manual: play Frieren S2 E1 in-app, seek, pause, back → Continue Watching shows it with progress; `~/.local/state/ani-cli/ani-hsts` has the line.
- [ ] **Step 5:** Commit `feat: in-window playback with progress and history sync`.

### Task 7: My List, Calendar, Downloads, Settings/Diagnostics pages

**REQUIRED SKILL for implementer:** `frontend-design`.

**Files:**
- Create: `src/ui/my_list.rs`, `src/ui/calendar.rs`, `src/ui/downloads_page.rs`, `src/ui/settings_page.rs`, `src/diagnostics.rs`
- Modify: `src/app.rs`

**Interfaces:**
- Consumes: `db`, `catalog::schedule_week`, `DownloadManager`, `anicli::{find, version}`, `player::MpvPlayer::new` (library probe).
- Produces: `diagnostics::Report { ani_cli: Option<(PathBuf, Option<String>)>, libmpv: Result<(), String>, ytdlp: bool, ffmpeg: bool, curl: bool }` + `diagnostics::run() -> Report`; `calendar::group_by_day(items: &[Airing], tz_offset_secs: i64) -> Vec<(String /*"Mon 24"*/, Vec<Airing>)>`.
- Calendar items open the Show page via `ShowRef { anilist_id, title, allanime: None }` (Show page resolves AllAnime with `catalog::resolve_allanime`). Downloads page: live progress via `set_on_change`, Play (local file → Player), Cancel, Retry, Remove, Open folder (`xdg-open`). Settings persist via `db.save_settings` on change; download dir chosen by text input (no file dialog).

- [ ] **Step 1: Failing tests:** `group_by_day_orders_and_labels`, `diagnostics_reports_missing_binary` (internal `which_in(path_var, name)` with empty PATH → false).
- [ ] **Step 2:** `cargo test` → fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Full check passes; manual: Calendar shows 7 days, a download completes and plays.
- [ ] **Step 5:** Commit `feat: my list, calendar, downloads and settings pages`.

### Task 8: Packaging, Python removal, docs, CI

**Files:**
- Create: `Makefile`, `assets/ani-gui.desktop`, `assets/icons/ani-gui.svg`, `install.sh` (rewrite), `.github/workflows/ci.yml` (rewrite)
- Delete: `ani_gui/`, `pyproject.toml`, `ani-gui` (python launcher), `Formula/`, `packaging/`, `DISTRIBUTION.md`, `docs/index.html`, `.vscode/tasks.json` if Python-specific
- Modify: `README.md` (rewrite for Rust app: features, deps `sudo apt install libmpv2 libxkbcommon-x11-dev …`, build, `make install`), `CHANGELOG.md` (1.0.0 entry), `CONTRIBUTING.md` (cargo commands)

**Interfaces:** `make install PREFIX=$HOME/.local` installs `$(PREFIX)/bin/ani-gui`, `$(PREFIX)/share/applications/io.github.sweettooth05.AniGui.desktop`, `$(PREFIX)/share/icons/hicolor/scalable/apps/io.github.sweettooth05.AniGui.svg`; `make uninstall` removes them. Desktop file: `Name=ani-gui`, `Exec=ani-gui`, `Icon=io.github.sweettooth05.AniGui`, `Categories=AudioVideo;Video;Player;`, `StartupWMClass=` matching the GPUI app id set in `main.rs` (`WindowOptions.app_id`).

- [ ] **Step 1:** `desktop-file-validate assets/ani-gui.desktop` (if installed) → no errors.
- [ ] **Step 2:** Implement; `make install PREFIX=$(mktemp -d)` → the three files exist; `make uninstall PREFIX=…` removes them.
- [ ] **Step 3:** `git grep -nE "python|pipx|PyPI|brew" -- . ':!CHANGELOG.md' ':!docs/superpowers'` → no matches.
- [ ] **Step 4:** Full check passes.
- [ ] **Step 5:** Commit `chore: native packaging, remove python app, rewrite docs`.

---

## Revision R1 (2026-11-23): ani-cli provider switch — supersedes conflicting text above

ani-cli 5.1.8 searches hianime.at, not AllAnime (spec "Stream resolution", revised). Changes:

- `model::EpisodeRequest` becomes `{ query: String, want_titles: Vec<String>, select_index: Option<usize>, ep: String, mode: Mode, quality: String }` (no `nth`).
- `db`: schema v2 adds `provider_titles(show_id TEXT PRIMARY KEY, title TEXT NOT NULL)`; `Db::get_provider_title(show_id) -> Result<Option<String>>`, `Db::set_provider_title(show_id, title) -> Result<()>`.
- `anicli`:
  - `ensure_shims(cache_dir: &Path, exe: &Path) -> io::Result<PathBuf /*shim dir*/>` replaces `ensure_capture_shim` (writes `capture-mpv`, `menu`, symlinks `rofi`, `dmenu`).
  - `build_request(show: &ShowSummary, provider_title: Option<&str>, ep: &str, mode: Mode, quality: &str) -> EpisodeRequest` — pure, no network. query = provider_title, else english_title, else title; want_titles = [provider_title?, english_title?, title] de-duplicated.
  - `enum ResolveOutcome { Stream(Stream), NeedsPick(Vec<String>) }`; `resolve(req: &EpisodeRequest, shims: &Path) -> anyhow::Result<ResolveOutcome>` (retry once with romaji `title` when output contains "No results found" and query was the English title).
  - `menu_main(args: &[String], stdin: &str, want_titles: &[String], out_file: Option<&Path>) -> String /*stdout to print*/` + `normalize_title(s) -> String`. `main.rs` dispatches `--ani-cli-menu` to it (reading env `ANI_GUI_WANT_TITLES`, `ANI_GUI_MENU_OUT`, stdin) before starting GPUI.
  - Tests: `menu_picks_exact_normalized_title` (stdin `1 Frieren: Beyond Journey's End\n2 Frieren: Beyond Journey's End Season 2`, want `Frieren: Beyond Journey’s End` → prints line 1), `menu_no_match_writes_candidates` (JSON array of the 2 titles in order; prints ""), `menu_postplay_prompt_prints_nothing`, `build_request_prefers_provider_title`, `resolve_needs_pick_when_menu_out_written` (fake ani-cli script writes the out file and exits 1), `resolve_retries_romaji_on_no_results`. Live `#[ignore]` `live_resolve_frieren` now resolves `{english: "Frieren: Beyond Journey's End", ep "1"}` with no `-S`.
- `history`: keep `read`; **remove `record`** (ani-cli writes history itself with its own ids).
- `downloads::DownloadManager::start(show: &ShowSummary, req: EpisodeRequest, dir: &Path)` uses the same shims/env with `-d`; a `NeedsPick` result marks the download Failed with error "Pick the matching title on the show page first".
- Task 5: `view_models::continue_items` matches history entries to shows by title (provider title table first, then normalized equality); Show page handles `NeedsPick` with a "Which one is it?" chooser that calls `set_provider_title` and re-resolves with `select_index`.
- Task 6: drop `playback::history_entry` and the `history::record` call; ani-cli records history during resolve.
