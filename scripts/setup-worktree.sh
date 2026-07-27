#!/bin/sh

set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
build_cli=true

if [ "${1:-}" = "--dev-prerequisites-only" ]; then
  build_cli=false
elif [ "$#" -gt 0 ]; then
  echo "Usage: $0 [--dev-prerequisites-only]" >&2
  exit 2
fi

for command in node npm cargo rustc git curl shasum; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "Required command is missing: $command" >&2
    exit 1
  fi
done

cd "$repo_root"

dependencies_pid=
dependency_stamp="$repo_root/.dakia-node-modules.sha256"
lockfile_sha256=$(shasum -a 256 "$repo_root/package-lock.json" | awk '{print $1}')
dependency_fingerprint="$lockfile_sha256 $(node --version)"
installed_dependency_fingerprint=
if [ -f "$dependency_stamp" ]; then
  installed_dependency_fingerprint=$(sed -n '1p' "$dependency_stamp")
fi

if [ ! -x "$repo_root/node_modules/.bin/tauri" ] ||
  [ "$installed_dependency_fingerprint" != "$dependency_fingerprint" ]; then
  echo "Installing locked JavaScript dependencies..."
  npm ci &
  dependencies_pid=$!
fi

"$repo_root/scripts/prepare-desktop-assets.sh" &
assets_pid=$!

status=0
if [ -n "$dependencies_pid" ]; then
  if wait "$dependencies_pid"; then
    printf '%s\n' "$dependency_fingerprint" >"$dependency_stamp"
  else
    status=1
  fi
fi
wait "$assets_pid" || status=1

if [ "$status" -ne 0 ]; then
  exit "$status"
fi

if [ "$build_cli" = true ]; then
  npm run bundle:cli:dev
fi

echo "Dakia worktree is ready."
