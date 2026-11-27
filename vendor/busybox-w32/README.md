# busybox-w32

`busybox-w64u-FRP-6354-g5f09fb446.exe` is an unmodified 64-bit Unicode
busybox-w32 build (Windows 10 1903+), downloaded from
<https://frippery.org/files/busybox/> and matching its `SHA256SUM`:

```
36b266923fc5765b42f5f981319f39568498fba60b53ccf26e8fc1e575e2a9a9
```

The Windows installer ships it as `lib\ani-gui\bin\busybox.exe`, and the app
runs ani-cli with its `sh`. It is kept in the repository because
frippery.org intermittently refuses downloads from GitHub's CI runners
(HTTP 403). `packaging/windows/build-windows.sh` checks the hash before using it.

busybox-w32 is licensed under GPL-2.0. Its source, including this release
(tag `FRP-6354`), is at <https://github.com/rmyorston/busybox-w32> and
<https://frippery.org/busybox/>.

To update it: download a newer versioned `busybox-w64u-*.exe` from
frippery.org, check it against their `SHA256SUM`, replace this file, and update
`BUSYBOX` and `BUSYBOX_SHA256` in `packaging/windows/build-windows.sh`.
