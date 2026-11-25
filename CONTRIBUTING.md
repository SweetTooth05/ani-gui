# Contributing to ani-gui

Thanks for your interest! ani-gui is a native Linux desktop app for
[ani-cli](https://github.com/pystardust/ani-cli), written in Rust with GPUI.
Contributions of all sizes are welcome: bug reports, docs, and code.

## Ground rules

- **ani-cli finds the streams.** ani-gui hands ani-cli a capture shim instead of
  a player and plays what it gets. Never re-implement stream extraction.
- **Keep the UI thread free.** Network, disk and process work run on
  `cx.background_spawn`; views update when the result arrives.
- **No `unwrap()`/`expect()` on fallible I/O, network or parse paths** outside
  tests. Errors go to the UI.
- **libmpv is loaded at runtime** (`libmpv.so.2` via `libloading`), never
  linked, so a missing library is an error message, not a crash.

## Getting started

Install the build and runtime dependencies listed in the
[README](README.md#dependencies), then:

```sh
git clone <your-fork>
cd ani-gui
cargo run
```

You need a working `ani-cli` and `libmpv2` to test playback.

## Before opening a PR

Run the checks CI runs:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
desktop-file-validate assets/ani-gui.desktop
shellcheck install.sh
```

Tests that hit live services or need the installed ani-cli are `#[ignore]`d;
run them with `cargo test -- --ignored` when you change that code.

- Keep changes focused; describe what and why in the PR.
- Update `CHANGELOG.md` under `## [Unreleased]`.
- If you change behavior, note it in the `README.md`.

## Reporting bugs

Use the issue templates. Include your distro, the commit you built
from (`git rev-parse --short HEAD`), `ani-cli -V`,
what Settings → Diagnostics shows, and the terminal output of `ani-gui` around
the error.

## Code style

`rustfmt` defaults, clippy clean. Small, readable functions; modules split by
responsibility (see `src/`).

By contributing you agree your work is licensed under the project's
[GPL-3.0](LICENSE).
