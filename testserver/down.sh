#!/bin/sh
# Stops the test server and removes its container (and with it the fixtures).
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
docker compose -f "$repo/testserver/compose.yaml" down
