#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
exec node "$root/scripts/run-large-mailbox-tls-smoke.mjs" "$@"
