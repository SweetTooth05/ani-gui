<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/svg/ani-gui-horizontal-reversed.svg">
    <img src="assets/brand/svg/ani-gui-horizontal.svg" alt="ani-gui" width="420">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/SweetTooth05/ani-gui/actions/workflows/ci.yml"><img src="https://github.com/SweetTooth05/ani-gui/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-GPL--3.0-blue" alt="license: GPL-3.0"></a>
</p>

A native Linux desktop app for watching anime, built in Rust with
[GPUI](https://www.gpui.rs/). It browses AllAnime and AniList, plays episodes
inside its own window through libmpv, and uses
[ani-cli](https://github.com/pystardust/ani-cli) to find streams. Because
ani-cli does the stream extraction, ani-gui keeps working as ani-cli updates.

## Features

- **Home**: a hero banner, Continue Watching, recommendations based on what you
  watch, and AniList's trending shows. Opens from the local cache straight away
  and refreshes in the background.
- **Search** with cover art and sub/dub episode counts.
- **Show pages** with the synopsis, genres, score, and an episode list that
  marks what you've watched.
- **In-window playback** through libmpv: seek bar, volume, fullscreen, external
  subtitle and audio tracks, autoplay of the next episode, and resume from where
  you stopped.
- **My List** for the shows you want to keep track of, plus your
  MyAnimeList and/or AniList list grouped by status.
- **MyAnimeList and AniList sync** (optional): imports what you've watched,
  marks episodes you finish, and sends a 1–10 rating when you finish a show.
  See [Connecting MyAnimeList and AniList](#connecting-myanimelist-and-anilist).
- **Calendar** of this week's airing episodes, from AniList.
- **Downloads** manager with live progress, cancel and retry.
- **Shares history with ani-cli**: what you watch here shows up in
  `ani-cli -c`, and the other way round.
- Settings for sub/dub, quality, download folder and autoplay, plus a
  Diagnostics panel that checks your dependencies.

### Player keys

| Key | Action |
|---|---|
| Space | Play / pause |
| ← / → | Seek back / forward |
| ↑ / ↓ | Volume |
| M | Mute |
| F | Fullscreen |
| N | Next episode |
| Esc | Leave fullscreen, then the player |
| Alt+← | Back (anywhere in the app) |

## Requirements

Linux on X11 or Wayland, with a GPU that supports Vulkan.

### Runtime

| Package | Why |
|---|---|
| [`ani-cli`](https://github.com/pystardust/ani-cli) | Finds the stream for each episode (it needs `curl`, `grep` and `sed`, plus `fzf` to use it in a terminal) |
| `libmpv2` | Plays video inside the window. Loaded when the app starts; without it, everything except playback still works |
| `yt-dlp` and `ffmpeg` | Downloads (ani-cli calls them) |

On Debian or Ubuntu:

```sh
sudo apt install libmpv2 curl fzf yt-dlp ffmpeg
```

ani-cli isn't packaged on most distros. Install it from the
[upstream repo](https://github.com/pystardust/ani-cli#installation), or:

```sh
mkdir -p ~/.local/bin
curl -sL https://raw.githubusercontent.com/pystardust/ani-cli/master/ani-cli -o ~/.local/bin/ani-cli
chmod +x ~/.local/bin/ani-cli
```

### Build

A recent stable Rust toolchain (from [rustup](https://rustup.rs)) and these
development packages:

```sh
sudo apt install build-essential pkg-config libxkbcommon-x11-dev libxkbcommon-dev \
  libwayland-dev libvulkan-dev libfontconfig-dev libfreetype-dev \
  libxcb1-dev
```

libmpv isn't needed to build, because the app loads it at runtime.

## Install

### From the latest release (recommended)

One command, no root and no build tools. It installs into `~/.local`:

```sh
v=$(curl -fsSL https://api.github.com/repos/SweetTooth05/ani-gui/releases/latest \
      | sed -n 's/.*"tag_name": *"v\([0-9.]*\)".*/\1/p') &&
  [ -n "$v" ] && mkdir -p ~/.local &&
  curl -fL "https://github.com/SweetTooth05/ani-gui/releases/download/v$v/ani-gui-$v-x86_64-unknown-linux-gnu.tar.gz" \
    | tar -xz -C ~/.local --strip-components=1 &&
  sed -i "s|^Exec=ani-gui\$|Exec=$HOME/.local/bin/ani-gui|" \
    ~/.local/share/applications/io.github.sweettooth05.AniGui.desktop
```

The tarball is laid out like an install prefix (`bin/`, `share/`), so this
puts the binary, the launcher entry and the icons in the same places as
`make install` below. The last step points the launcher entry at
`~/.local/bin/ani-gui`, because that folder is often missing from the desktop
session's `PATH`. To check a download's signature by hand first, see
[docs/releasing.md](docs/releasing.md#verifying-a-release-by-hand).

### From source

```sh
git clone https://github.com/SweetTooth05/ani-gui.git
cd ani-gui
make
make install
```

This installs into `~/.local` and needs no root:

- `~/.local/bin/ani-gui`
- `~/.local/share/applications/io.github.sweettooth05.AniGui.desktop`
- `~/.local/share/icons/hicolor/*/apps/io.github.sweettooth05.AniGui*` (PNG sizes 16–512, scalable and symbolic SVG)

ani-gui then shows up in your app launcher. The launcher entry runs
`~/.local/bin/ani-gui` by its full path.

To install system-wide instead, build as your user and install as root, so cargo
never runs as root:

```sh
make
sudo make install PREFIX=/usr/local
```

`./install.sh` does the same as `make && make install` after checking that the
build tools and libraries are there.

### Updates

ani-gui updates itself when it is installed in `~/.local`, from the release
tarball or with `make install`:

- It checks GitHub's latest release a few seconds after it starts, at most
  once every six hours. **Settings → Updates → Check now** checks straight
  away.
- It installs a newer version only if the release's checksums carry a valid
  signature from the ani-gui release key built into the app, and the
  download matches them.
- It replaces `~/.local/bin/ani-gui` in place. The new version runs the next
  time you start ani-gui, or straight away with **Restart now**.
- Turn off **Install updates automatically** to only be told about new
  versions.

It never asks for root. A system-wide install such as `/usr/local`, or a
build run from the source tree (`cargo run`, `target/release/`), only tells
you when an update is out. Update those with `git pull && make && make install`
(add `sudo` and `PREFIX=...` for a system-wide install).

### Uninstall

```sh
make uninstall                       # or: sudo make uninstall PREFIX=/usr/local
```

This removes the binary, the desktop entry and the icons. Your library and
settings stay where they are (see below); delete those folders too for a
clean slate.

## Connecting MyAnimeList and AniList

Connect either account, or both, in **Settings → Accounts**. ani-gui then:

- imports your anime list when you connect, when the app starts (at most every
  half hour) and when you press **Sync now**, and shows it on My List grouped
  by status. Episodes up to your list's progress count as watched on show
  pages, and shows you're watching appear in Continue Watching;
- updates your progress on each connected service when you finish an episode
  (marking the show completed after its final episode), and offers a 1–10
  rating then and in the show page's **Your list** section;
- keeps changes it couldn't send (offline, server down) and retries them on the
  next sync. When both lists have the same show, the most recently updated one
  wins.

There are no built-in developer keys: you register your own (free) API client
with each service and paste its client ID into Settings. Client IDs aren't
secret. Sign-in tokens are stored only in your system keychain (Secret Service,
such as GNOME Keyring or KWallet, on Linux; the Keychain on macOS), never in
ani-gui's files. Without a working keychain, connecting fails with an
explanation rather than saving tokens anywhere else.

### MyAnimeList

1. Sign in at [myanimelist.net/apiconfig](https://myanimelist.net/apiconfig)
   and choose **Create ID**.
2. Set **App Type** to **other**, and **App Redirect URL** to exactly:
   ```
   http://localhost:53682/callback
   ```
   Fill in the other required fields (name, description, homepage) however you
   like.
3. Copy the **Client ID** (not the secret: "other" apps don't have one) into
   Settings → Accounts → MyAnimeList, and press **Connect**.
4. Approve access in the browser that opens. ani-gui listens on
   `127.0.0.1:53682` for up to five minutes to receive the redirect; when the
   page says you can close the tab, you're connected.

The sign-in uses OAuth 2 with PKCE, and the token refreshes itself. If port
53682 is taken by another program, close it and try again.

### AniList

1. Go to [anilist.co/settings/developer](https://anilist.co/settings/developer)
   and choose **Create New Client**.
2. Set the **Redirect URL** to exactly:
   ```
   https://anilist.co/api/v2/oauth/pin
   ```
3. Copy the client's **ID** (a number) into Settings → Accounts → AniList, and
   press **Connect**.
4. Approve access in the browser. AniList then shows a token: copy all of it,
   paste it into the field ani-gui shows, and press **Save token**.

AniList tokens last a year. When one expires or you revoke access, ani-gui
marks the account disconnected and asks you to connect again.

**Disconnect** removes the token from the keychain and forgets the imported
list; your lists on MyAnimeList and AniList are untouched.

## Where your data lives

| What | Where |
|---|---|
| Library, watch progress, My List, downloads list, settings, imported MyAnimeList/AniList lists (SQLite) | `~/.local/share/ani-gui/ani-gui.db` (`$XDG_DATA_HOME/ani-gui/`) |
| MyAnimeList / AniList sign-in tokens | Your system keychain (service `ani-gui`) |
| Cover art and the helper scripts ani-cli calls | `~/.cache/ani-gui/` (`$XDG_CACHE_HOME/ani-gui/`) |
| Downloaded episodes | `~/Videos/ani-gui` by default; change it in Settings |
| Watch history shared with ani-cli | `~/.local/state/ani-cli/ani-hsts` (ani-cli's own file; `$ANI_CLI_HIST_DIR` and `$XDG_STATE_HOME` are honoured) |

The cache is safe to delete at any time.

## Troubleshooting

Start with **Settings → Diagnostics**. It shows whether ani-cli (and its
version), libmpv, yt-dlp, ffmpeg and curl were found. Click **Check again**
after installing something.

- **"Install libmpv2" when you press Play**: the app couldn't load
  `libmpv.so.2`. Install `libmpv2` (Debian/Ubuntu) or your distro's mpv library
  package.
- **An episode won't start, or shows ani-cli's error**: run
  `ani-cli <show name>` in a terminal. If that fails too, the problem is
  upstream; update ani-cli (`ani-cli -U`) or check its
  [issues](https://github.com/pystardust/ani-cli/issues).
- **"Which one is it?" on a show page**: ani-cli's search returned several
  similar titles. Pick the right one once and ani-gui remembers it for that
  show.
- **Downloads fail straight away**: check that `yt-dlp` and `ffmpeg` are
  installed, and that the download folder in Settings is writable.
- **The window doesn't open**: run `ani-gui` from a terminal to see the error.
  GPUI needs a working Vulkan driver (`vulkaninfo` from `vulkan-tools` will
  tell you).
- **Rows on Home are empty**: AniList may be rate-limiting you, or you're
  offline. Cached rows still show; the rest come back on the next refresh.

## Development

```sh
cargo run                                     # debug build
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

See [CONTRIBUTING.md](CONTRIBUTING.md). The logo files and their usage rules
are in [`assets/brand/`](assets/brand/README.md).

## License

[GPL-3.0](LICENSE). ani-gui is a front end for ani-cli and doesn't host any
content.
