#!/bin/sh
# Wraps a built ani-gui.app in a disk image: open it, drag ani-gui onto the
# Applications shortcut beside it, done. macOS only (hdiutil).
#
#   packaging/macos/build-dmg.sh APP OUTFILE
set -eu

if [ "$#" -ne 2 ]; then
	echo "usage: $0 APP OUTFILE" >&2
	exit 2
fi
app=$1
out=$2
[ -d "$app/Contents/MacOS" ] || { echo "build-dmg.sh: $app is not an app bundle" >&2; exit 1; }

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
# ditto keeps the bundle's signature and symlinks intact.
ditto "$app" "$stage/ani-gui.app"
ln -s /Applications "$stage/Applications"
rm -f "$out"
hdiutil create -volname ani-gui -srcfolder "$stage" -fs HFS+ -format UDZO -ov "$out"
hdiutil verify "$out"
