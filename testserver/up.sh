#!/bin/sh
# Starts the SFTP test server on 127.0.0.1:2222 and waits until it is ready.
#
#   user ferry, password ferry
#   keys .testserver/id_ed25519 (no passphrase) and
#        .testserver/id_ed25519_encrypted (passphrase "ferry")
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
state="$repo/.testserver"
mkdir -p "$state"

if [ ! -f "$state/id_ed25519" ]; then
    ssh-keygen -q -t ed25519 -N '' -C ferry-testserver -f "$state/id_ed25519"
fi
if [ ! -f "$state/id_ed25519_encrypted" ]; then
    ssh-keygen -q -t ed25519 -N 'ferry' -C ferry-testserver-encrypted -f "$state/id_ed25519_encrypted"
fi
cat "$state/id_ed25519.pub" "$state/id_ed25519_encrypted.pub" > "$state/authorized_keys"

docker compose -f "$repo/testserver/compose.yaml" up --detach --build --wait
echo "Ferry test server ready: sftp -P 2222 -i .testserver/id_ed25519 ferry@127.0.0.1"
