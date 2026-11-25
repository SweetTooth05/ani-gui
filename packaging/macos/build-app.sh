#!/bin/sh
# Builds ani-gui.app from a release binary. macOS only (it needs sips,
# iconutil, plutil and codesign, which ship with macOS).
#
#   packaging/macos/build-app.sh BINARY OUTDIR
#
# writes OUTDIR/ani-gui.app, replacing any bundle already there:
#   Contents/MacOS/ani-gui              the binary
#   Contents/Info.plist                 from packaging/macos/Info.plist
#   Contents/Resources/AppIcon.icns     from assets/icons/hicolor
#   Contents/Resources/bin/ani-cli      packaging/fetch-ani-cli.sh
#   Contents/Frameworks/*.dylib         libmpv and its libraries
# and signs it ad hoc. `make app` runs it on target/release/ani-gui. It
# needs Homebrew's mpv and dylibbundler on the build machine only.
set -eu

if [ "$#" -ne 2 ]; then
	echo "usage: $0 BINARY OUTDIR" >&2
	exit 2
fi
bin=$1
outdir=$2

if [ "$(uname -s)" != Darwin ]; then
	echo "build-app.sh: ani-gui.app can only be built on macOS" >&2
	exit 1
fi
if [ ! -x "$bin" ]; then
	echo "build-app.sh: $bin is not an executable; run cargo build --release first" >&2
	exit 1
fi

root=$(cd "$(dirname "$0")/../.." && pwd)
# The first `version =` in Cargo.toml is the [package] version.
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)
case $version in
	[0-9]*.[0-9]*.[0-9]*) ;;
	*)
		echo "build-app.sh: no version found in Cargo.toml" >&2
		exit 1
		;;
esac

app="$outdir/ani-gui.app"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin" "$app/Contents/MacOS/ani-gui"
chmod 755 "$app/Contents/MacOS/ani-gui"
sed "s/@VERSION@/$version/g" "$root/packaging/macos/Info.plist" > "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist"

# iconutil wants 16, 32, 128, 256 and 512 pt icons at 1x and 2x. The
# hicolor set stops at 512 px, so the 1024 px one is the 512 scaled up.
icons="$root/assets/icons/hicolor"
iconset="$work/AppIcon.iconset"
mkdir "$iconset"
png() {
	echo "$icons/${1}x${1}/apps/io.github.sweettooth05.AniGui.png"
}
for size in 16 32 128 256 512; do
	cp "$(png "$size")" "$iconset/icon_${size}x${size}.png"
done
cp "$(png 32)" "$iconset/icon_16x16@2x.png"
cp "$(png 64)" "$iconset/icon_32x32@2x.png"
cp "$(png 256)" "$iconset/icon_128x128@2x.png"
cp "$(png 512)" "$iconset/icon_256x256@2x.png"
sips -z 1024 1024 "$(png 512)" --out "$iconset/icon_512x512@2x.png" >/dev/null
iconutil -c icns -o "$app/Contents/Resources/AppIcon.icns" "$iconset"

# libmpv and everything it links, so the app plays video without Homebrew
# on the user's Mac. They are copied from the build machine's Homebrew mpv
# (`brew install mpv dylibbundler`) into Contents/Frameworks, where the
# player looks first (src/player/mod.rs), and repointed at each other.
fw="$app/Contents/Frameworks"
mkdir -p "$fw"
brew_prefix=$(brew --prefix 2>/dev/null) || {
	echo "build-app.sh: Homebrew not found; run 'brew install mpv dylibbundler'" >&2
	exit 1
}
libmpv="$brew_prefix/lib/libmpv.2.dylib"
[ -e "$libmpv" ] || { echo "build-app.sh: $libmpv missing; run 'brew install mpv'" >&2; exit 1; }
command -v dylibbundler >/dev/null || { echo "build-app.sh: run 'brew install dylibbundler'" >&2; exit 1; }
cp -L "$libmpv" "$fw/libmpv.2.dylib"
chmod 644 "$fw/libmpv.2.dylib"
install_name_tool -id @loader_path/libmpv.2.dylib "$fw/libmpv.2.dylib"
dylibbundler -of -b -x "$fw/libmpv.2.dylib" -d "$fw" -p @loader_path/ \
	-s "$brew_prefix/lib" >/dev/null
# Nothing may still point into Homebrew: that would only work on Macs that
# happen to have the same packages installed.
for lib in "$fw"/*.dylib; do
	if otool -L "$lib" | tail -n +2 | grep -E "^[[:space:]]*($brew_prefix|/usr/local|/opt/homebrew)/"; then
		echo "build-app.sh: $(basename "$lib") still links Homebrew paths" >&2
		exit 1
	fi
done

# ani-cli, for Macs without one installed (the app prefers the user's own).
"$root/packaging/fetch-ani-cli.sh" "$app/Contents/Resources/bin"
mv "$app/Contents/Resources/bin/ani-cli-LICENSE" "$app/Contents/Resources/ani-cli-LICENSE"

# The bundled libraries target the build machine's macOS (Homebrew builds
# for the release it runs on), so the app can't claim an older minimum.
min=11.0
for macho in "$app/Contents/MacOS/ani-gui" "$fw"/*.dylib; do
	v=$(otool -l "$macho" | awk '/LC_BUILD_VERSION/{b=1} b&&/minos/{print $2; exit}
		/LC_VERSION_MIN_MACOSX/{m=1} m&&/version/{print $2; exit}')
	[ -n "$v" ] && min=$(printf '%s\n%s\n' "$min" "$v" | sort -t. -k1,1n -k2,2n | tail -n 1)
done
plutil -replace LSMinimumSystemVersion -string "$min" "$app/Contents/Info.plist"

# Ad hoc: no Developer ID, so the app isn't notarized (see the README).
# Libraries first, then the bundle, which seals them.
for lib in "$fw"/*.dylib; do
	codesign --force -s - "$lib"
done
codesign --force --deep -s - "$app"
codesign --verify --deep --strict "$app"
echo "Built $app (ani-gui $version, macOS $min or later, $(find "$fw" -name "*.dylib" | wc -l | tr -d ' ') bundled libraries)"
