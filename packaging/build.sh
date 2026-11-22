#!/bin/sh
# Builds ferge for distribution, as target/dist/ferge.
#
# Binaries carry the source paths of panic locations, which name the build
# machine's home directory (and with it the user name) for every dependency.
# They are rewritten to neutral prefixes here, and the build fails if the
# binary still contains the home directory or the checkout's path.
#
# The flags given here replace RUSTFLAGS and any rustflags from Cargo
# configuration, so local settings such as `-C target-cpu=native` never end
# up in a binary that is handed out.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd -P)
home=$(cd "$HOME" && pwd -P)
cargo_home=$(cd "${CARGO_HOME:-$home/.cargo}" && pwd -P)
rustup_home=$(cd "${RUSTUP_HOME:-$home/.rustup}" && pwd -P)

# Arguments separated by 0x1f, so paths with spaces stay whole. When several
# prefixes match, the last one wins: the most specific comes last.
sep=$(printf '\037')
flags="--remap-path-prefix=$home=/home"
flags="$flags$sep--remap-path-prefix=$rustup_home=/rustup"
flags="$flags$sep--remap-path-prefix=$cargo_home=/cargo"
flags="$flags$sep--remap-path-prefix=$root=/ferge"
export CARGO_ENCODED_RUSTFLAGS="$flags"
unset RUSTFLAGS

cd "$root"
cargo build --locked --profile dist -p ferge-app

binary="$root/target/dist/ferge"
if grep -a -q -F -e "$home" -e "$root" "$binary"; then
    echo "error: $binary still contains $home or $root" >&2
    exit 1
fi
echo "built $binary ($(du -h "$binary" | cut -f1))"
