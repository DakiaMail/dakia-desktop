#!/bin/sh

set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

"$repo_root/scripts/prepare-lfs-assets.sh" &
lfs_pid=$!
"$repo_root/scripts/prepare-onnx-runtime.sh" &
runtime_pid=$!

status=0
wait "$lfs_pid" || status=1
wait "$runtime_pid" || status=1
exit "$status"
