#!/bin/sh
# Builds ferge for distribution and prints the binary's path:
# target/dist/ferge, or target/TRIPLE/dist/ferge with `--target TRIPLE`.
#
# Binaries carry the source paths of panic locations, which name the build
# machine's home directory (and with it the user name) for every dependency.
# They are rewritten to neutral prefixes here, and the build fails if the
# binary still contains a path in the checkout, the Rust installation or a
# hidden folder of the home directory.
#
# The flags given here replace RUSTFLAGS and any rustflags from Cargo
# configuration, so local settings such as `-C target-cpu=native` never end
# up in a binary that is handed out.
set -eu

target=
case $# in
0) ;;
2)
    [ "$1" = --target ] || { echo "usage: $0 [--target TRIPLE]" >&2; exit 2; }
    target=$2
    ;;
*) echo "usage: $0 [--target TRIPLE]" >&2; exit 2 ;;
esac

root=$(cd "$(dirname "$0")/.." && pwd -P)
home=$(cd "$HOME" && pwd -P)

# Arguments separated by 0x1f, so paths with spaces stay whole. When several
# prefixes match, the last one wins: the most specific comes last.
sep=$(printf '\037')
flags="--remap-path-prefix=$home=/home"
# The directories whose paths must not reach the binary, checked below.
leaks="$home/.${sep}$root/crates/${sep}$root/target/"
remap() {
    if [ -d "$1" ]; then
        dir=$(cd "$1" && pwd -P)
        flags="$flags$sep--remap-path-prefix=$dir=$2"
        leaks="$leaks$sep$dir/"
    fi
}
remap "${RUSTUP_HOME:-$home/.rustup}" /rustup
remap "${CARGO_HOME:-$home/.cargo}" /cargo
flags="$flags$sep--remap-path-prefix=$root=/ferge"
export CARGO_ENCODED_RUSTFLAGS="$flags"
unset RUSTFLAGS

cd "$root"
if [ -n "$target" ]; then
    cargo build --locked --profile dist -p ferge-app --target "$target" >&2
    binary="$root/target/$target/dist/ferge"
else
    cargo build --locked --profile dist -p ferge-app >&2
    binary="$root/target/dist/ferge"
fi

# Only these exact prefixes: the home directory or the checkout as such can
# be part of unrelated text, such as /root in an accessibility interface name.
old_ifs=$IFS
IFS=$sep
set -f
for leak in $leaks; do
    if LC_ALL=C grep -a -q -F -e "$leak" "$binary"; then
        echo "error: $binary still contains $leak" >&2
        exit 1
    fi
done
IFS=$old_ifs
set +f
echo "$binary"
