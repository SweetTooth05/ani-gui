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
# and signs it ad hoc. `make app` runs it on target/release/ani-gui.
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

# Ad hoc: no Developer ID, so the app isn't notarized (see the README).
codesign --force --deep -s - "$app"
codesign --verify --strict "$app"
echo "Built $app (ani-gui $version)"
