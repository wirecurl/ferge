#!/bin/sh
# Release helpers, used by .github/workflows/release.yml:
#
#   packaging/release.sh version      prints the version from Cargo.toml
#   packaging/release.sh check TAG    fails unless the tag, Cargo.toml, CHANGELOG.md
#                                     and the AppStream metainfo agree on the release
#   packaging/release.sh notes        prints the CHANGELOG.md section of the version
set -eu

root=$(cd "$(dirname "$0")/.." && pwd -P)
metainfo="$root/packaging/linux/com.wirecurl.ferge.metainfo.xml"

die() {
    echo "error: $*" >&2
    exit 1
}

version() {
    # The first `version = "…"` of Cargo.toml is the one of [workspace.package].
    v=$(sed -n 's/^version = "\([^"]*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)
    printf '%s\n' "$v" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$' ||
        die "no usable version in Cargo.toml: '$v'"
    printf '%s\n' "$v"
}

# The date of the version's heading in CHANGELOG.md, `## [X.Y.Z] - YYYY-MM-DD`.
changelog_date() {
    grep -F "## [$1] - " "$root/CHANGELOG.md" | head -n 1 |
        sed -n 's/^## \[[^]]*\] - \([0-9]\{4\}-[0-9]\{2\}-[0-9]\{2\}\)$/\1/p'
}

case ${1:-} in
version)
    version
    ;;
check)
    [ $# -eq 2 ] || die "usage: $0 check TAG"
    v=$(version)
    [ "$2" = "v$v" ] || die "tag $2 does not match version $v in Cargo.toml"
    date=$(changelog_date "$v")
    [ -n "$date" ] || die "CHANGELOG.md has no heading '## [$v] - YYYY-MM-DD'"
    grep -Fq "<release version=\"$v\" date=\"$date\"" "$metainfo" ||
        die "the metainfo has no <release version=\"$v\" date=\"$date\">"
    echo "release $v of $date"
    ;;
notes)
    v=$(version)
    awk -v heading="## [$v]" '
        index($0, heading) == 1 { inside = 1; next }
        inside && /^## \[/ { exit }
        inside && /^\[[^]]*\]: / { next }
        inside { print }
    ' "$root/CHANGELOG.md"
    ;;
*)
    die "usage: $0 version | check TAG | notes"
    ;;
esac
