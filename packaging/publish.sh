#!/bin/sh
# After a release is published on GitHub, points the package recipes at it:
#
#   packaging/publish.sh aur VERSION    sets version and checksums in packaging/aur/*
#                                       and writes their .SRCINFO (needs makepkg)
#   packaging/publish.sh cask VERSION   updates the cask in wirecurl/homebrew-tap and
#                                       pushes it (needs gh, logged in)
#
# Uploading the AUR recipes is left to the maintainer: copy PKGBUILD and
# .SRCINFO into the clone of ssh://aur@aur.archlinux.org/<name>.git and push.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd -P)
repo=wirecurl/ferge

die() {
    echo "error: $*" >&2
    exit 1
}

[ $# -eq 2 ] || die "usage: $0 aur|cask VERSION"
what=$1
version=$2
printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$' ||
    die "unusable version: $version"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

gh release download "v$version" --repo "$repo" --pattern SHA256SUMS --dir "$work"
# The checksum of an asset of the release, from its SHA256SUMS.
sum() {
    sum=$(awk -v name="$1" '$2 == name { print $1 }' "$work/SHA256SUMS")
    [ -n "$sum" ] || die "$1 is not part of release $version"
    printf '%s\n' "$sum"
}

case $what in
aur)
    command -v makepkg >/dev/null || die "makepkg is needed for .SRCINFO"
    curl -sSfL -o "$work/source.tar.gz" \
        "https://github.com/$repo/archive/refs/tags/v$version.tar.gz"
    source_sum=$(sha256sum "$work/source.tar.gz" | cut -d' ' -f1)
    x86_64_sum=$(sum "ferge-$version-x86_64-linux.tar.gz")
    aarch64_sum=$(sum "ferge-$version-aarch64-linux.tar.gz")

    for name in ferge ferge-bin; do
        recipe="$root/packaging/aur/$name/PKGBUILD"
        sed -i \
            -e "s/^pkgver=.*/pkgver=$version/" \
            -e "s/^pkgrel=.*/pkgrel=1/" \
            -e "s/^sha256sums=.*/sha256sums=('$source_sum')/" \
            -e "s/^sha256sums_x86_64=.*/sha256sums_x86_64=('$x86_64_sum')/" \
            -e "s/^sha256sums_aarch64=.*/sha256sums_aarch64=('$aarch64_sum')/" \
            "$recipe"
        (cd "$(dirname "$recipe")" && makepkg --printsrcinfo >.SRCINFO)
        echo "updated packaging/aur/$name"
    done
    ;;
cask)
    dmg_sum=$(sum "ferge-$version-macos.dmg")
    gh repo clone wirecurl/homebrew-tap "$work/tap" -- --quiet --depth 1
    mkdir -p "$work/tap/Casks"
    sed -e "s/@VERSION@/$version/" -e "s/@SHA256@/$dmg_sum/" \
        "$root/packaging/homebrew/ferge.rb.in" >"$work/tap/Casks/ferge.rb"
    cd "$work/tap"
    git add Casks/ferge.rb
    if git diff --cached --quiet; then
        echo "the cask is already at $version"
        exit 0
    fi
    # The same author as in this repository, not the global git identity.
    git -c user.name="$(git -C "$root" config user.name)" \
        -c user.email="$(git -C "$root" config user.email)" \
        commit -q -m "feat: ferge $version"
    git push -q origin HEAD
    echo "pushed ferge $version to wirecurl/homebrew-tap"
    ;;
*)
    die "usage: $0 aur|cask VERSION"
    ;;
esac
