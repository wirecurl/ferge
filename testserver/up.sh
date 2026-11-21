#!/bin/sh
# Starts the SFTP test server on 127.0.0.1:2222 and waits until it is ready.
#
#   user ferry, password ferry (sudo with password; ferry-nopasswd and
#   ferry-nosudo for the other cases)
#   keys .testserver/id_ed25519 (no passphrase) and
#        .testserver/id_ed25519_encrypted (passphrase "ferry")
#   client config .testserver/ssh_config (host alias "ferry-test")
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

# Client configuration for `ferry -F .testserver/ssh_config sftp://ferry-test`
# and for the benchmarks. The host keys change whenever the image is rebuilt.
ssh-keyscan -p 2222 -t ed25519,ecdsa,rsa 127.0.0.1 2>/dev/null > "$state/known_hosts"
cat > "$state/ssh_config" <<CONFIG
Host ferry-test
  HostName 127.0.0.1
  Port 2222
  User ferry
  IdentityFile $state/id_ed25519
  UserKnownHostsFile $state/known_hosts
CONFIG

echo "Ferry test server ready: cargo run -- -F .testserver/ssh_config sftp://ferry-test/home/ferry/many"
