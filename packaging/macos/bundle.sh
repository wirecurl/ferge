#!/bin/sh
# Wraps a ferge binary into ferge.app and a disk image:
#
#     packaging/macos/bundle.sh VERSION BINARY OUT
#
# creates OUT/ferge.app and OUT/ferge-VERSION-macos.dmg. The app is signed
# ad hoc, which Apple silicon requires to run it at all; without a Developer
# ID, Gatekeeper asks before the first start of a downloaded copy.
set -eu

[ $# -eq 3 ] || { echo "usage: $0 VERSION BINARY OUT" >&2; exit 2; }
version=$1
binary=$2
out=$3
here=$(cd "$(dirname "$0")" && pwd -P)

case $version in
*[!0-9A-Za-z.+-]* | "") echo "error: unusable version: $version" >&2; exit 2 ;;
esac

app="$out/ferge.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
sed "s/@VERSION@/$version/g" "$here/Info.plist" >"$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist" >/dev/null
cp "$binary" "$app/Contents/MacOS/ferge"
chmod 755 "$app/Contents/MacOS/ferge"
cp "$here/ferge.icns" "$app/Contents/Resources/ferge.icns"
printf 'APPL????' >"$app/Contents/PkgInfo"

codesign --force --sign - --timestamp=none "$app"
codesign --verify --strict "$app"

# The image holds the app and a link to /Applications to drag it onto.
stage="$out/dmg"
rm -rf "$stage"
mkdir -p "$stage"
cp -R "$app" "$stage/"
ln -s /Applications "$stage/Applications"
dmg="$out/ferge-$version-macos.dmg"
rm -f "$dmg"
hdiutil create -volname ferge -srcfolder "$stage" -fs HFS+ -format UDZO -ov "$dmg" >/dev/null
rm -rf "$stage"
echo "$dmg"
