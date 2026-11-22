#!/bin/sh
# Starts the SFTP test servers on 127.0.0.1:2222 (with fixtures) and
# 127.0.0.1:2223 (empty, for direct transfers between servers) and waits until
# they are ready. FERGE_TEST_PORT, FERGE_TEST_PORT2 and FERGE_TESTSERVER_NAME
# move them, e.g. to run a second set next to the first.
#
#   user ferge, password ferge (sudo with password; ferge-nopasswd and
#   ferge-nosudo for the other cases)
#   keys .testserver/id_ed25519 (no passphrase) and
#        .testserver/id_ed25519_encrypted (passphrase "ferge")
#   client config .testserver/ssh_config (host aliases "ferge-test" and
#   "ferge-test2")
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
state="$repo/.testserver"
port=${FERGE_TEST_PORT:-2222}
port2=${FERGE_TEST_PORT2:-2223}
mkdir -p "$state"

if [ ! -f "$state/id_ed25519" ]; then
    ssh-keygen -q -t ed25519 -N '' -C ferge-testserver -f "$state/id_ed25519"
fi
if [ ! -f "$state/id_ed25519_encrypted" ]; then
    ssh-keygen -q -t ed25519 -N 'ferge' -C ferge-testserver-encrypted -f "$state/id_ed25519_encrypted"
fi
cat "$state/id_ed25519.pub" "$state/id_ed25519_encrypted.pub" > "$state/authorized_keys"

docker compose -f "$repo/testserver/compose.yaml" up --detach --build --wait

# Client configuration for `ferge -F .testserver/ssh_config sftp://ferge-test`
# and for the benchmarks. The host keys change whenever the image is rebuilt.
{
    ssh-keyscan -p "$port" -t ed25519,ecdsa,rsa 127.0.0.1 2>/dev/null
    ssh-keyscan -p "$port2" -t ed25519,ecdsa,rsa 127.0.0.1 2>/dev/null
} > "$state/known_hosts"
cat > "$state/ssh_config" <<CONFIG
Host ferge-test
  HostName 127.0.0.1
  Port $port
  User ferge
  IdentityFile $state/id_ed25519
  UserKnownHostsFile $state/known_hosts

Host ferge-test2
  HostName 127.0.0.1
  Port $port2
  User ferge
  IdentityFile $state/id_ed25519
  UserKnownHostsFile $state/known_hosts
CONFIG

echo "ferge test servers ready: cargo run -- -F .testserver/ssh_config sftp://ferge-test/home/ferge/many"
echo "direct transfers between them: FERGE_DIRECT_ADDRESS=127.0.0.1:$port=sshd:22,127.0.0.1:$port2=sshd2:22"
