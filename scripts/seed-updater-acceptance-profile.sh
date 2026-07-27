#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
  echo "Usage: $0 /path/to/initialized-data-directory" >&2
  exit 2
fi

root_dir=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
exec python3 "$root_dir/scripts/updater-acceptance-profile.py" seed "$1"
