# Troubleshooting

Start with **Settings → Diagnostics**. It shows whether ani-cli (and its
version), libmpv, yt-dlp, ffmpeg and curl were found. Click **Check again**
after installing something.

- **"Install libmpv2" or missing `libmpv` when you press Play**:
  - **Linux**: the app couldn't load `libmpv.so.2`. Install `libmpv2`
    (Debian/Ubuntu) or your distro's mpv library package.
  - **Windows**: the app couldn't load `libmpv-2.dll`. The official installer
    and `.tar.gz` bundle it next to `ani-gui.exe`; reinstall, or (for source
    builds) place `libmpv-2.dll` from
    [shinchiro/mpv-winbuild-cmake](https://github.com/shinchiro/mpv-winbuild-cmake/releases)
    (`mpv-dev-x86_64-*.7z`) beside `ani-gui.exe`.
- **Windows says "Windows protected your PC" (SmartScreen)**: the Windows
  installer and binary aren't code-signed. Click **More info → Run anyway**
  to allow it.
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
  On Linux, GPUI needs a working Vulkan driver (`vulkaninfo` from
  `vulkan-tools` will tell you).
- **Rows on Home are empty**: AniList may be rate-limiting you, or you're
  offline. Cached rows still show; the rest come back on the next refresh.

