#!/bin/sh
# Downloads the ani-cli that the packaged builds ship, pinned by commit and
# checked against its SHA-256, plus ani-cli's licence.
#
#   packaging/fetch-ani-cli.sh OUTDIR
#
# writes OUTDIR/ani-cli (executable) and OUTDIR/ani-cli-LICENSE. The app
# prefers an ani-cli the user installed; this copy is the fallback, so the
# .dmg, .deb and AppImage work without installing it separately.
set -eu

# ani-cli 5.1.8. To update: pick a commit on pystardust/ani-cli master, then
# set both values from `curl -fsSL <url> | sha256sum`.
COMMIT=7867d3b35d7a0cb6b589b33373cf4a954d4c4036
SHA256=24ada974b567789da4ea51f9f92ec5b975b43fc0e999543f9053bcf5a628105c
LICENSE_SHA256=3972dc9744f6499f0f9b2dbf76696f2ae7ad8af9b23dde66d6af86c9dfb36986

if [ "$#" -ne 1 ]; then
	echo "usage: $0 OUTDIR" >&2
	exit 2
fi
out=$1
base="https://raw.githubusercontent.com/pystardust/ani-cli/$COMMIT"

sha256() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | cut -d' ' -f1
	else
		shasum -a 256 "$1" | cut -d' ' -f1
	fi
}

fetch() { # URL FILE EXPECTED_SHA256
	curl -fsSL --retry 3 "$1" -o "$2.tmp"
	got=$(sha256 "$2.tmp")
	if [ "$got" != "$3" ]; then
		rm -f "$2.tmp"
		echo "fetch-ani-cli.sh: $1 has SHA-256 $got, expected $3" >&2
		exit 1
	fi
	mv "$2.tmp" "$2"
}

mkdir -p "$out"
fetch "$base/ani-cli" "$out/ani-cli" "$SHA256"
chmod 755 "$out/ani-cli"
fetch "$base/LICENSE" "$out/ani-cli-LICENSE" "$LICENSE_SHA256"
chmod 644 "$out/ani-cli-LICENSE"
