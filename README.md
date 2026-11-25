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

**Docs:** [Features and player keys](docs/features.md) ·
[Install guide](docs/install.md) (updates, uninstalling, macOS) ·
[MyAnimeList and AniList](docs/accounts.md) ·
[Where your data lives](docs/data.md) ·
[Troubleshooting](docs/troubleshooting.md) ·
[Contributing](CONTRIBUTING.md)

## Dependencies

**Linux** on X11 or Wayland with a Vulkan-capable GPU, plus:

| Package | Why |
|---|---|
| [`ani-cli`](https://github.com/pystardust/ani-cli) | Finds each episode's stream (needs `curl`, `grep`, `sed`; `fzf` for terminal use) |
| `libmpv2` | Plays video in the window; everything else works without it |
| `yt-dlp`, `ffmpeg` | Downloads |

```sh
sudo apt install libmpv2 curl fzf yt-dlp ffmpeg
# ani-cli isn't packaged on most distros:
mkdir -p ~/.local/bin
curl -sL https://raw.githubusercontent.com/pystardust/ani-cli/master/ani-cli -o ~/.local/bin/ani-cli
chmod +x ~/.local/bin/ani-cli
```

**macOS** 11 or later: `brew install mpv ani-cli yt-dlp ffmpeg`.

Building from source also needs [Rust](https://rustup.rs) and, on Linux:

```sh
sudo apt install build-essential pkg-config libxkbcommon-x11-dev libxkbcommon-dev \
  libwayland-dev libvulkan-dev libfontconfig-dev libfreetype-dev libxcb1-dev
```

## Install

From the latest release, into `~/.local` (no root, no build tools):

```sh
v=$(curl -fsSL https://api.github.com/repos/SweetTooth05/ani-gui/releases/latest \
      | sed -n 's/.*"tag_name": *"v\([0-9.]*\)".*/\1/p') &&
  [ -n "$v" ] && mkdir -p ~/.local &&
  curl -fL "https://github.com/SweetTooth05/ani-gui/releases/download/v$v/ani-gui-$v-x86_64-unknown-linux-gnu.tar.gz" \
    | tar -xz -C ~/.local --strip-components=1 &&
  sed -i "s|^Exec=ani-gui\$|Exec=$HOME/.local/bin/ani-gui|" \
    ~/.local/share/applications/io.github.sweettooth05.AniGui.desktop &&
  { gtk-update-icon-cache -f -t ~/.local/share/icons/hicolor
    update-desktop-database ~/.local/share/applications; } >/dev/null 2>&1; true
```

Or from source:

```sh
git clone https://github.com/SweetTooth05/ani-gui.git
cd ani-gui
make && make install
```

ani-gui then appears in your app menu and keeps itself up to date from
GitHub Releases. For macOS, system-wide installs, updates and uninstalling,
see the [install guide](docs/install.md).

## License

[GPL-3.0](LICENSE). ani-gui is a front end for ani-cli and doesn't host any
content.
