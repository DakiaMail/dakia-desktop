#!/bin/sh

set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

# Development builds are re-signed after every compilation, which makes macOS
# Keychain ask for permission again. Load ignored, user-only overrides so local
# development never touches Keychain; packaged builds still use it normally.
if [ -f "$repo_root/.env" ]; then
  set -a
  . "$repo_root/.env"
  set +a
fi

export CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER=../../../scripts/codesign-dev-runner.sh
export CARGO_TARGET_X86_64_APPLE_DARWIN_RUNNER=../../../scripts/codesign-dev-runner.sh

cd "$repo_root"
"$repo_root/scripts/setup-worktree.sh" --dev-prerequisites-only
exec tauri dev --config apps/desktop/src-tauri/tauri.conf.json
