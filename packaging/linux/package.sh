#!/bin/sh
# Packages a ferge binary for Linux:
#
#     packaging/linux/package.sh VERSION ARCH BINARY OUT
#
# ARCH is x86_64 or aarch64. Creates in OUT:
#
#   ferge-VERSION-ARCH-linux.tar.gz  a prefix tree (bin/, share/) that works
#                                    unpacked into ~/.local or /usr/local
#   ferge_VERSION_DEBARCH.deb        with nfpm, if it is on the PATH
#   ferge-VERSION-1.ARCH.rpm         likewise
set -eu

[ $# -eq 4 ] || { echo "usage: $0 VERSION ARCH BINARY OUT" >&2; exit 2; }
version=$1
arch=$2
binary=$3
out=$(mkdir -p "$4" && cd "$4" && pwd -P)
here=$(cd "$(dirname "$0")" && pwd -P)
root=$(cd "$here/../.." && pwd -P)

case $version in
*[!0-9A-Za-z.+~-]* | "") echo "error: unusable version: $version" >&2; exit 2 ;;
esac
case $arch in
x86_64) debarch=amd64 ;;
aarch64) debarch=arm64 ;;
*) echo "error: unknown architecture: $arch" >&2; exit 2 ;;
esac

name="ferge-$version-$arch-linux"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
stage="$work/$name"

install -Dm755 "$binary" "$stage/bin/ferge"
install -Dm644 "$here/com.wirecurl.ferge.desktop" \
    "$stage/share/applications/com.wirecurl.ferge.desktop"
install -Dm644 "$here/com.wirecurl.ferge.metainfo.xml" \
    "$stage/share/metainfo/com.wirecurl.ferge.metainfo.xml"
(cd "$here/icons" && find hicolor -type f) | while read -r icon; do
    install -Dm644 "$here/icons/$icon" "$stage/share/icons/$icon"
done
install -Dm644 "$root/README.md" "$stage/share/doc/ferge/README.md"
install -Dm644 "$root/CHANGELOG.md" "$stage/share/doc/ferge/CHANGELOG.md"
install -Dm644 "$root/LICENSE" "$stage/share/licenses/ferge/LICENSE"

# Fixed owner, order and times, so the same input gives the same archive.
tar --sort=name --owner=0 --group=0 --numeric-owner \
    --mtime="@${SOURCE_DATE_EPOCH:-0}" -C "$work" -cf - "$name" |
    gzip -n -9 >"$out/$name.tar.gz"
echo "$out/$name.tar.gz"

if ! command -v nfpm >/dev/null; then
    echo "nfpm not found: no .deb and .rpm" >&2
    exit 0
fi

# Every file is listed on its own: a whole tree would make the packages own
# directories such as /usr/share/icons, which belong to the system.
config="$work/nfpm.yaml"
{
    cat <<EOF
name: ferge
version: "$version"
arch: $debarch
platform: linux
section: net
priority: optional
maintainer: wirecurl <andreas@wirecurl.com>
vendor: wirecurl
homepage: https://github.com/wirecurl/ferge
license: GPL-3.0-or-later
description: |-
  Fast, keyboard-first file transfer client.
  ferge moves files between this computer and SFTP servers, and between two
  servers, with a transfer queue, previews and working as root through sudo.
overrides:
  deb:
    depends: ["libc6 (>= 2.35)", libgcc-s1, libxcb1, libxkbcommon0, libxkbcommon-x11-0,
      libwayland-client0, libwayland-egl1, libvulkan1]
    recommends: [mesa-vulkan-drivers]
  # By soname, so that one package fits Fedora, openSUSE and the rest; the
  # (64bit) mark holds for aarch64 as well. glibc 2.35 is what it is built on.
  rpm:
    depends: ["libc.so.6(GLIBC_2.35)(64bit)", "libxcb.so.1()(64bit)", "libxkbcommon.so.0()(64bit)",
      "libxkbcommon-x11.so.0()(64bit)", "libwayland-client.so.0()(64bit)",
      "libwayland-egl.so.1()(64bit)", "libvulkan.so.1()(64bit)"]
    recommends: [mesa-vulkan-drivers]
contents:
  - src: $stage/bin/ferge
    dst: /usr/bin/ferge
    file_info: {mode: 0755}
EOF
    (cd "$stage/share" && find . -type f | sort) | while read -r file; do
        file=${file#./}
        printf '  - src: %s\n    dst: /usr/share/%s\n    file_info: {mode: 0644}\n' \
            "$stage/share/$file" "$file"
    done
} >"$config"

nfpm package --config "$config" --packager deb --target "$out/" >&2
nfpm package --config "$config" --packager rpm --target "$out/" >&2
ls "$out"/*.deb "$out"/*.rpm
