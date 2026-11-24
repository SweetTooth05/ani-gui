#!/bin/sh
# Build ani-gui from this checkout and install it for the current user.
#
#   ./install.sh                      installs into ~/.local
#   PREFIX=~/apps ./install.sh        any other prefix you can write to
#
# For a system-wide install, don't run this script with sudo (it would build
# as root); run `make && sudo make install PREFIX=/usr/local` instead.
#
# This is a thin wrapper around `make && make install`; see README.md for the
# packages it expects.
set -eu

say()  { printf '\033[36m==>\033[0m %s\n' "$1"; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$1" >&2; }
die()  { printf '\033[31merror:\033[0m %s\n' "$1" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

[ "$(id -u)" -ne 0 ] || die "don't run this as root; use 'make && sudo make install PREFIX=/usr/local'"

cd "$(dirname "$0")"
if [ ! -f Cargo.toml ] || [ ! -f Makefile ]; then
  die "run this script from an ani-gui checkout"
fi

have cargo || die "cargo not found; install Rust from https://rustup.rs"
have make  || die "make not found; install your distro's build tools (e.g. build-essential)"
have cc    || die "no C compiler found; install your distro's build tools (e.g. build-essential)"
have pkg-config || die "pkg-config not found; see the build dependencies in README.md"
pkg-config --exists xkbcommon-x11 fontconfig wayland-client \
  || die "missing development libraries; see the build dependencies in README.md"

# Runtime dependencies: the build works without them, the app does not.
have ani-cli || warn "ani-cli not found; ani-gui needs it to find streams"
{ ldconfig -p 2>/dev/null || /sbin/ldconfig -p 2>/dev/null; } | grep -q 'libmpv\.so\.2' \
  || warn "libmpv.so.2 not found; install libmpv2 to play video"
have yt-dlp || warn "yt-dlp not found; downloads need yt-dlp and ffmpeg"

PREFIX=${PREFIX:-$HOME/.local}
say "building (cargo build --release)"
make build
say "installing into $PREFIX"
make install PREFIX="$PREFIX"

case ":$PATH:" in
  *":$PREFIX/bin:"*) : ;;
  *) warn "$PREFIX/bin is not on your PATH; add it (or log out and back in) so the launcher entry can start ani-gui" ;;
esac
say "done; launch ani-gui from your app menu or run 'ani-gui'"
