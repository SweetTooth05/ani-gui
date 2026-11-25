#!/bin/sh
# Builds ani-gui-linux-amd64.deb from a release binary, for Debian, Ubuntu
# and their derivatives. Double-clicking it opens the distribution's software
# installer, which also installs libmpv and the other dependencies.
#
#   packaging/linux/build-deb.sh BINARY ANI_CLI_DIR OUTDIR
#
# ANI_CLI_DIR holds ani-cli and ani-cli-LICENSE (packaging/fetch-ani-cli.sh).
# The package installs into /usr, so the app can't replace itself there: it
# tells the user about new versions instead (src/updater.rs).
set -eu

if [ "$#" -ne 3 ]; then
	echo "usage: $0 BINARY ANI_CLI_DIR OUTDIR" >&2
	exit 2
fi
bin=$1
ani_cli_dir=$2
outdir=$3
root=$(cd "$(dirname "$0")/../.." && pwd)

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)
case $version in
	[0-9]*.[0-9]*.[0-9]*) ;;
	*)
		echo "build-deb.sh: no version found in Cargo.toml" >&2
		exit 1
		;;
esac

# The newest glibc symbol the binary uses sets the minimum libc6.
glibc=$(objdump -T "$bin" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -n 1)
[ -n "$glibc" ] || { echo "build-deb.sh: no glibc symbols in $bin" >&2; exit 1; }

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
pkg="$stage/pkg"

# The binary, launcher entry and icons, as `make install` lays them out.
make -C "$root" --no-print-directory install DESTDIR="$pkg" PREFIX=/usr \
	DESKTOP_EXEC=ani-gui BIN="$(cd "$(dirname "$bin")" && pwd)/$(basename "$bin")"
install -Dm755 "$ani_cli_dir/ani-cli" "$pkg/usr/lib/ani-gui/bin/ani-cli"

doc="$pkg/usr/share/doc/ani-gui"
install -d "$doc"
{
	echo "ani-gui: https://github.com/SweetTooth05/ani-gui"
	echo "License: GPL-3.0-only. Full text: /usr/share/common-licenses/GPL-3"
	echo
	echo "Bundled: ani-cli (/usr/lib/ani-gui/bin/ani-cli),"
	echo "https://github.com/pystardust/ani-cli, licensed as follows:"
	echo
	cat "$ani_cli_dir/ani-cli-LICENSE"
} > "$doc/copyright"
chmod 644 "$doc/copyright"

size=$(du -sk "$pkg/usr" | cut -f1)
install -d "$pkg/DEBIAN"
# libwayland, libvulkan, fontconfig and freetype are opened at run time by
# GPUI, and libmpv by the player, so they aren't in the binary's NEEDED list.
cat > "$pkg/DEBIAN/control" <<EOF
Package: ani-gui
Version: $version
Architecture: amd64
Maintainer: SweetTooth05 <SweetTooth05@users.noreply.github.com>
Installed-Size: $size
Depends: libc6 (>= $glibc), libgcc-s1, libxcb1, libxkbcommon0, libxkbcommon-x11-0, libwayland-client0, libvulkan1, libfontconfig1, libfreetype6, libmpv2 | libmpv1, curl, grep, sed
Recommends: yt-dlp, ffmpeg
Section: video
Priority: optional
Homepage: https://github.com/SweetTooth05/ani-gui
Description: Desktop app for watching anime with ani-cli
 Browses AllAnime and AniList and plays episodes in its own window through
 libmpv. Streams are found with ani-cli; a copy is included.
EOF

mkdir -p "$outdir"
out="$outdir/ani-gui-linux-amd64.deb"
dpkg-deb --root-owner-group -Zxz --build "$pkg" "$out"
dpkg-deb --info "$out"
dpkg-deb --contents "$out"
