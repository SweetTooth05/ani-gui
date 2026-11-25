#!/bin/sh
# Builds ani-gui-linux-x86_64.AppImage from a release binary: one file that
# runs on most distributions, with libmpv and ani-cli inside. Build it on the
# oldest distribution it should run on (glibc is not bundled).
#
#   packaging/linux/build-appimage.sh BINARY ANI_CLI_DIR OUTDIR
#
# ANI_CLI_DIR holds ani-cli (packaging/fetch-ani-cli.sh). Needs curl, make
# and the system's libmpv (libmpv-dev); linuxdeploy is downloaded, pinned.
set -eu

# linuxdeploy and its AppImage plugin, pinned by release and SHA-256.
LINUXDEPLOY_URL=https://github.com/linuxdeploy/linuxdeploy/releases/download/1-alpha-20251107-1/linuxdeploy-x86_64.AppImage
LINUXDEPLOY_SHA256=c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d
PLUGIN_URL=https://github.com/linuxdeploy/linuxdeploy-plugin-appimage/releases/download/1-alpha-20250213-1/linuxdeploy-plugin-appimage-x86_64.AppImage
PLUGIN_SHA256=992d502a248e14ab185448ddf6f6e7d25558cb84d4623c354c3af350c25fccb3

if [ "$#" -ne 3 ]; then
	echo "usage: $0 BINARY ANI_CLI_DIR OUTDIR" >&2
	exit 2
fi
bin=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
ani_cli_dir=$2
outdir=$(mkdir -p "$3" && cd "$3" && pwd)
root=$(cd "$(dirname "$0")/../.." && pwd)
app_id=io.github.sweettooth05.AniGui

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)
[ -n "$version" ] || { echo "build-appimage.sh: no version in Cargo.toml" >&2; exit 1; }

# The player loads libmpv.so.2, else libmpv.so.1 (src/player/mod.rs).
libmpv=$(ldconfig -p | sed -n 's/.*libmpv\.so\.[12] .*=> //p' | sort | tail -n 1)
[ -n "$libmpv" ] || { echo "build-appimage.sh: libmpv not found; install libmpv-dev" >&2; exit 1; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

fetch() { # URL FILE SHA256
	curl -fsSL --retry 3 "$1" -o "$2"
	echo "$3  $2" | sha256sum -c - >/dev/null || {
		echo "build-appimage.sh: $1 doesn't match its pinned SHA-256" >&2
		exit 1
	}
	chmod 755 "$2"
}
fetch "$LINUXDEPLOY_URL" "$work/linuxdeploy-x86_64.AppImage" "$LINUXDEPLOY_SHA256"
fetch "$PLUGIN_URL" "$work/linuxdeploy-plugin-appimage-x86_64.AppImage" "$PLUGIN_SHA256"

appdir="$work/AppDir"
make -C "$root" --no-print-directory install DESTDIR="$appdir" PREFIX=/usr \
	DESKTOP_EXEC=ani-gui BIN="$bin"
install -Dm755 "$ani_cli_dir/ani-cli" "$appdir/usr/lib/ani-gui/bin/ani-cli"

# linuxdeploy copies libmpv and its dependencies into usr/lib (leaving out
# the base system libraries every distribution has), where the player looks
# first, and points the copies at each other. libxkbcommon stays the
# system's: an older copy can't read a newer system's keyboard data.
cd "$work"
APPIMAGE_EXTRACT_AND_RUN=1 ARCH=x86_64 LINUXDEPLOY_OUTPUT_VERSION="$version" \
	LDAI_OUTPUT="$outdir/ani-gui-linux-x86_64.AppImage" PATH="$work:$PATH" \
	./linuxdeploy-x86_64.AppImage --appdir "$appdir" \
	--executable "$appdir/usr/bin/ani-gui" \
	--library "$libmpv" \
	--exclude-library 'libxkbcommon*' \
	--desktop-file "$appdir/usr/share/applications/$app_id.desktop" \
	--icon-file "$appdir/usr/share/icons/hicolor/256x256/apps/$app_id.png" \
	--output appimage
ls -l "$outdir/ani-gui-linux-x86_64.AppImage"
