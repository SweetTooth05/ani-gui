#!/bin/sh
# Builds the Windows x64 release archive and Inno Setup 6 installer from a
# release binary. Run under Git Bash on Windows (needs curl, sha256sum, tar,
# 7z, iscc, cygpath, and powershell.exe for the libmpv smoke check).
#
#   packaging/windows/build-windows.sh BINARY OUTDIR
#
# Writes into OUTDIR:
#   ani-gui-<version>-x86_64-pc-windows-msvc.tar.gz   prefix tree (portable + updater)
#   ani-gui-windows-x86_64-setup.exe                 per-user Inno Setup installer
set -eu

TARGET=x86_64-pc-windows-msvc

# shinchiro/mpv-winbuild-cmake baseline x86_64 libmpv (not -v3, so it runs on
# pre-AVX2 CPUs). To update: pick a release from
# https://github.com/shinchiro/mpv-winbuild-cmake/releases and compute
# `curl -fsSL <url> | sha256sum`.
LIBMPV_URL=https://github.com/shinchiro/mpv-winbuild-cmake/releases/download/20261111/mpv-dev-x86_64-20261111-git-bcde6b93cc.7z
LIBMPV_SHA256=273c079b11ea5575822f972756f4c9a71149a5c10130353abeb8586738938452

# busybox-w32 64-bit Unicode build (Windows 10 1903+ / Windows 11). To update:
# pick a versioned binary from https://frippery.org/files/busybox/ and verify
# against its SHA256SUM.
BUSYBOX_URL=https://frippery.org/files/busybox/busybox-w64u-FRP-6354-g5f09fb446.exe
BUSYBOX_SHA256=36b266923fc5765b42f5f981319f39568498fba60b53ccf26e8fc1e575e2a9a9

if [ "$#" -ne 2 ]; then
	echo "usage: $0 BINARY OUTDIR" >&2
	exit 2
fi
if [ ! -f "$1" ]; then
	echo "build-windows.sh: $1 not found; run cargo build --release first" >&2
	exit 1
fi

bin=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
outdir=$(mkdir -p "$2" && cd "$2" && pwd)
root=$(cd "$(dirname "$0")/../.." && pwd)

# The first `version =` in Cargo.toml is the [package] version.
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)
case $version in
	[0-9]*.[0-9]*.[0-9]*) ;;
	*)
		echo "build-windows.sh: no version found in Cargo.toml" >&2
		exit 1
		;;
esac

find_tool() { # NAME CANDIDATE...
	name=$1
	shift
	if command -v "$name" >/dev/null 2>&1; then
		command -v "$name"
		return 0
	fi
	for candidate in "$@"; do
		if [ -x "$candidate" ]; then
			printf '%s\n' "$candidate"
			return 0
		fi
	done
	return 1
}

sevenz=$(find_tool 7z \
	"/c/Program Files/7-Zip/7z.exe" \
	"/c/Program Files (x86)/7-Zip/7z.exe") || {
	echo "build-windows.sh: 7z not found; install 7-Zip" >&2
	exit 1
}
iscc=$(find_tool iscc \
	"/c/Program Files (x86)/Inno Setup 6/ISCC.exe" \
	"/c/Program Files/Inno Setup 6/ISCC.exe") || {
	echo "build-windows.sh: iscc not found; install Inno Setup 6 (e.g. choco install innosetup)" >&2
	exit 1
}

winpath() {
	if command -v cygpath >/dev/null 2>&1; then
		cygpath -w "$1"
	else
		printf '%s\n' "$1"
	fi
}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

fetch() { # URL FILE EXPECTED_SHA256
	if ! curl -fsSL --retry 3 "$1" -o "$2.tmp"; then
		echo "build-windows.sh: downloading $1 failed" >&2
		exit 1
	fi
	got=$(sha256sum "$2.tmp" | cut -d' ' -f1)
	if [ "$got" != "$3" ]; then
		rm -f "$2.tmp"
		echo "build-windows.sh: $1 has SHA-256 $got, expected $3" >&2
		exit 1
	fi
	mv "$2.tmp" "$2"
}

stem="ani-gui-${version}-${TARGET}"
prefix="$work/stage/$stem"
mkdir -p "$prefix/bin" "$prefix/lib/ani-gui/bin" "$prefix/share/icons" "$prefix/share/doc/ani-gui"

cp "$bin" "$prefix/bin/ani-gui.exe"

fetch "$LIBMPV_URL" "$work/mpv-dev.7z" "$LIBMPV_SHA256"
MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*' "$sevenz" e -y \
	"$(winpath "$work/mpv-dev.7z")" "-o$(winpath "$prefix/bin")" libmpv-2.dll >/dev/null

fetch "$BUSYBOX_URL" "$prefix/lib/ani-gui/bin/busybox.exe" "$BUSYBOX_SHA256"
chmod 755 "$prefix/lib/ani-gui/bin/busybox.exe"

sh "$root/packaging/fetch-ani-cli.sh" "$work/ani-cli"
cp "$work/ani-cli/ani-cli" "$prefix/lib/ani-gui/bin/ani-cli"
chmod 755 "$prefix/lib/ani-gui/bin/ani-cli"

cp "$root/assets/icons/ani-gui.ico" "$prefix/share/icons/ani-gui.ico"
cp "$root/LICENSE" "$prefix/share/doc/ani-gui/LICENSE"
cp "$work/ani-cli/ani-cli-LICENSE" "$prefix/share/doc/ani-gui/ani-cli-LICENSE"

# Smoke checks on the staged prefix tree.
for rel in bin/ani-gui.exe bin/libmpv-2.dll lib/ani-gui/bin/ani-cli lib/ani-gui/bin/busybox.exe; do
	if [ ! -s "$prefix/$rel" ]; then
		echo "build-windows.sh: staged file missing or empty: $rel" >&2
		exit 1
	fi
done

# ani-cli checks for a player before it handles -V; the app passes
# ANI_CLI_PLAYER=debug for the same reason (anicli::version).
ver_out=$(
	cd "$prefix" &&
		ANI_CLI_PLAYER=debug MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*' \
			./lib/ani-gui/bin/busybox.exe sh lib/ani-gui/bin/ani-cli -V
)
echo "Smoke check (ani-cli -V): $ver_out"
printf '%s\n' "$ver_out" | grep -Eq '[0-9]+\.[0-9]+' || {
	echo "build-windows.sh: unexpected ani-cli -V output: $ver_out" >&2
	exit 1
}

cat > "$work/check-libmpv.ps1" <<'EOF'
param([Parameter(Mandatory = $true)][string]$DllPath)
$ErrorActionPreference = "Stop"
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class NativeLibCheck {
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern IntPtr LoadLibraryW(string lpFileName);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool FreeLibrary(IntPtr hModule);
}
'@
$h = [NativeLibCheck]::LoadLibraryW($DllPath)
if ($h -eq [IntPtr]::Zero) {
    $err = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
    throw "LoadLibraryW failed for $DllPath (Win32 error $err)"
}
[void][NativeLibCheck]::FreeLibrary($h)
Write-Host "Smoke check (libmpv-2.dll): LoadLibraryW OK"
EOF
MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*' powershell.exe \
	-NoProfile -NonInteractive -ExecutionPolicy Bypass \
	-File "$(winpath "$work/check-libmpv.ps1")" -DllPath "$(winpath "$prefix/bin/libmpv-2.dll")"

# Portable archive & in-app updater payload: single top-level directory matching the archive stem.
tar_out="$outdir/$stem.tar.gz"
if tar --version 2>/dev/null | grep -q 'GNU tar'; then
	tar --force-local --sort=name --owner=0 --group=0 --numeric-owner \
		-czf "$tar_out" -C "$work/stage" "$stem"
	tar --force-local -tzvf "$tar_out"
	archive_list=$(tar --force-local -tzf "$tar_out")
else
	tar -czf "$tar_out" -C "$work/stage" "$stem"
	tar -tzvf "$tar_out"
	archive_list=$(tar -tzf "$tar_out")
fi

if printf '%s\n' "$archive_list" | grep -v "^${stem}/" | grep -q .; then
	echo "build-windows.sh: archive has entries outside $stem/" >&2
	exit 1
fi
for rel in bin/ani-gui.exe bin/libmpv-2.dll lib/ani-gui/bin/ani-cli lib/ani-gui/bin/busybox.exe; do
	if ! printf '%s\n' "$archive_list" | grep -qxF "${stem}/${rel}"; then
		echo "build-windows.sh: archive missing ${stem}/${rel}" >&2
		exit 1
	fi
done

# Per-user Inno Setup 6 installer (fixed filename for releases/latest/download/).
MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*' "$iscc" \
	"/DAppVersion=$version" \
	"/DStageDir=$(winpath "$prefix")" \
	"/O$(winpath "$outdir")" \
	"$(winpath "$root/packaging/windows/ani-gui.iss")"

setup_out="$outdir/ani-gui-windows-x86_64-setup.exe"
if [ ! -s "$setup_out" ]; then
	echo "build-windows.sh: expected installer $setup_out was not created" >&2
	exit 1
fi
ls -lh "$tar_out" "$setup_out"
