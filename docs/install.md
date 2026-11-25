# Installing ani-gui

The quickest way is a download button in the [README](../README.md#install).
This page covers the details and the other ways: Linux first, then
[macOS](#macos).

## Linux

### .deb and AppImage

- **`ani-gui-linux-amd64.deb`** (Debian, Ubuntu 22.04+, Mint, Pop!_OS):
  double-click it, or `sudo apt install ./ani-gui-linux-amd64.deb`. It
  installs into `/usr` and pulls in libmpv, curl and the other libraries;
  yt-dlp and ffmpeg (for downloads) come as recommendations. Remove it with
  `sudo apt remove ani-gui`.
- **`ani-gui-linux-x86_64.AppImage`** (any distribution with glibc 2.35 or
  newer): libmpv and its libraries are inside. Make it executable
  (`chmod +x`, or Properties → Executable as Program) and run it. Delete the
  file to remove it.

Both include ani-cli, used when you haven't installed your own. Neither can
replace itself (the .deb lives in `/usr`, the AppImage is read-only), so
ani-gui tells you when a new version is out and links to the download.

### Into `~/.local` from the terminal

One command, no root and no build tools, and ani-gui then updates itself
(see [Updates](#updates)). You need the [dependencies](#dependencies) below,
except ani-cli, which the archive includes:

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

The tarball is laid out like an install prefix (`bin/`, `share/`), so this
puts the binary, the launcher entry and the icons in the same places as
`make install` below. The `sed` step points the launcher entry at
`~/.local/bin/ani-gui`, because that folder is often missing from the desktop
session's `PATH`. The last step refreshes the icon and launcher caches, as
`make install` does: `tar` keeps the archive's folder dates, so an icon cache
another app left in `~/.local/share/icons` would otherwise look up to date
and hide ani-gui's icon. To check a download's signature by hand first, see
[releasing.md](releasing.md#verifying-a-release-by-hand).

### Dependencies

For the `~/.local` install and builds from source (the .deb and AppImage
bring their own). Linux on X11 or Wayland with a Vulkan-capable GPU, plus:

| Package | Why |
|---|---|
| `libmpv2` (or `libmpv1`) | Plays video in the window; everything else works without it |
| `curl`, `grep`, `sed` | Used by ani-cli |
| `yt-dlp`, `ffmpeg` | Downloads |
| [`ani-cli`](https://github.com/pystardust/ani-cli) | Finds each episode's stream; included in the release archive, needed for source builds |

```sh
sudo apt install libmpv2 curl yt-dlp ffmpeg
# ani-cli, for source builds (it isn't packaged on most distros):
mkdir -p ~/.local/bin
curl -sL https://raw.githubusercontent.com/pystardust/ani-cli/master/ani-cli -o ~/.local/bin/ani-cli
chmod +x ~/.local/bin/ani-cli
```

### From source

A recent stable [Rust](https://rustup.rs) and these development packages:

```sh
sudo apt install build-essential pkg-config libxkbcommon-x11-dev libxkbcommon-dev \
  libwayland-dev libvulkan-dev libfontconfig-dev libfreetype-dev libxcb1-dev
```

Then:

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
settings stay where they are (see [where your data lives](data.md)); delete those folders too for a
clean slate.


## macOS

ani-gui runs on Apple Silicon and Intel Macs, on macOS 15 or later.
It comes as an `ani-gui.app` bundle with libmpv (video playback) and ani-cli
inside, so it needs nothing else: no Homebrew.

### From a release

Download `ani-gui-macos-arm64.dmg` (Apple Silicon) or
`ani-gui-macos-x86_64.dmg` (Intel) with the [README](../README.md#install)'s
buttons or from the
[latest release](https://github.com/SweetTooth05/ani-gui/releases/latest),
open it and drag **ani-gui** onto **Applications**. Then see
[the first launch](#the-first-launch-gatekeeper).

The same app also comes as `ani-gui-<version>-<arch>-apple-darwin.tar.gz`,
for `tar -xzf … -C ~/Applications` in a terminal.

If you have your own ani-cli (for example `brew install ani-cli`), ani-gui
uses it instead of the built-in one. Downloading episodes needs `yt-dlp` and
`ffmpeg` (`brew install yt-dlp ffmpeg`); watching doesn't.

### From source

You need [rustup](https://rustup.rs), the Xcode Command Line Tools
(`xcode-select --install`) and, to bundle libmpv into the app, Homebrew's
`brew install mpv dylibbundler` (on the build machine only). Then:

```sh
git clone https://github.com/SweetTooth05/ani-gui.git
cd ani-gui
make app
cp -R target/release/ani-gui.app ~/Applications/
```

`make app` builds the release binary and wraps it in
`target/release/ani-gui.app` with its icon, libmpv and ani-cli, signed ad
hoc. `packaging/macos/build-dmg.sh target/release/ani-gui.app ani-gui.dmg`
makes the disk image.

### The first launch (Gatekeeper)

The app is signed ad hoc, not with an Apple Developer ID, and it is not
notarized by Apple. When you download it in a browser, macOS can block it the
first time with a message that it "cannot be opened" or "cannot be
verified". To allow it:

- On macOS 14 and earlier, right-click `ani-gui.app` in Finder and choose
  **Open**, then **Open** again.
- On macOS 15 and later, try to open it once, then go to **System Settings →
  Privacy & Security** and click **Open Anyway**.
- On any version, you can instead remove the download quarantine flag in
  Terminal:

  ```sh
  xattr -dr com.apple.quarantine ~/Applications/ani-gui.app
  ```

You only need to do this once. A copy you built yourself, or downloaded with
`curl` rather than a browser, carries no quarantine flag and opens straight
away.

### Updates on macOS

ani-gui updates the whole `ani-gui.app` bundle in place, with the same
signature checks as on Linux. It needs to be able to write to the folder that
holds the app:

- In `~/Applications`, automatic updates work without an administrator
  account.
- In `/Applications`, they work only if your account can write there (on a
  standard Mac, administrator accounts can). Otherwise ani-gui only tells you
  when a new version is out.
- An app started from the download folder before you move it (macOS then
  runs it from a read-only copy) or from `target/release/` also only tells
  you.

On macOS, your library and settings live in
`~/Library/Application Support/ani-gui/` and the cache in
`~/Library/Caches/ani-gui/`. Downloads go to `~/Movies/ani-gui` by default.
ani-cli's watch history is in the same place as on Linux. To uninstall, drag
`ani-gui.app` to the Bin.

