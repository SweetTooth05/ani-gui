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

## Install

<p align="center">
  <a href="https://github.com/SweetTooth05/ani-gui/releases/latest/download/ani-gui-macos-arm64.dmg"><img src="https://img.shields.io/badge/Download-macOS%20(Apple%20Silicon)-111?style=for-the-badge&logo=apple&logoColor=white" alt="Download for macOS (Apple Silicon)"></a>
  <a href="https://github.com/SweetTooth05/ani-gui/releases/latest/download/ani-gui-macos-x86_64.dmg"><img src="https://img.shields.io/badge/Download-macOS%20(Intel)-111?style=for-the-badge&logo=apple&logoColor=white" alt="Download for macOS (Intel)"></a>
  <br>
  <a href="https://github.com/SweetTooth05/ani-gui/releases/latest/download/ani-gui-linux-amd64.deb"><img src="https://img.shields.io/badge/Download-Linux%20(.deb)-E95420?style=for-the-badge&logo=ubuntu&logoColor=white" alt="Download for Linux (.deb)"></a>
  <a href="https://github.com/SweetTooth05/ani-gui/releases/latest/download/ani-gui-linux-x86_64.AppImage"><img src="https://img.shields.io/badge/Download-Linux%20(AppImage)-333?style=for-the-badge&logo=linux&logoColor=white" alt="Download for Linux (AppImage)"></a>
</p>

**macOS 15 or later** (Apple menu → About This Mac tells you Apple Silicon or Intel):
open the `.dmg` and drag **ani-gui** onto **Applications**. The first time,
macOS says it can't verify the app (it isn't notarized by Apple): open it once,
then click **Open Anyway** in **System Settings → Privacy & Security**.
Nothing else to install; video playback and ani-cli are built in.

**Ubuntu, Debian, Mint, Pop!_OS**: double-click the `.deb` and choose
**Install**. It installs what ani-gui needs (libmpv and friends) with it.

**Other Linux distributions**: download the AppImage, right-click it →
**Properties** → allow **Executable as Program**, then double-click it.

All downloads include ani-cli. The app keeps itself up to date (the `.deb` and
AppImage tell you when a new version is out). More options, such as installing
into `~/.local` from the terminal or building from source, are in the
[install guide](docs/install.md).

## Dependencies

None for the downloads above, on a desktop with a Vulkan-capable GPU (Linux)
or macOS. Optional extras:

| Package | Why |
|---|---|
| `yt-dlp`, `ffmpeg` | Downloading episodes to watch offline (the `.deb` suggests them) |
| [`ani-cli`](https://github.com/pystardust/ani-cli) | Your own copy is used instead of the built-in one, so `ani-cli -U` can update it when a site changes |

Building from source needs [Rust](https://rustup.rs) and, on Linux, the
packages listed in the [install guide](docs/install.md#from-source).

## License

[GPL-3.0](LICENSE). ani-gui is a front end for ani-cli and doesn't host any
content.
