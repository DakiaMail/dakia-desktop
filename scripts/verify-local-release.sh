#!/usr/bin/env bash
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root_dir"

# This is the release-Mac gate, not a contributor-only test. Using the actual
# Developer ID identity keeps the app and ONNX dylib on one Apple Team ID.
# shellcheck source=local-release-env.sh
source "$root_dir/scripts/local-release-env.sh"
dakia_require_signing_environment

npm run setup:worktree
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
npm run typecheck
npm run format:check
npm run test
npm run test:release-scripts
npm run build:web

APPLE_SIGNING_IDENTITY="$APPLE_SIGNING_IDENTITY" \
ORT_LIB_LOCATION="$root_dir/apps/desktop/src-tauri/frameworks" \
ORT_PREFER_DYNAMIC_LINK=1 \
  npm run tauri -- build --bundles app \
    --config apps/desktop/src-tauri/tauri.conf.json \
    --config apps/desktop/src-tauri/tauri.ci.conf.json
"$root_dir/scripts/verify-macos-release-app.sh" \
  "$root_dir/target/release/bundle/macos/Dakia.app"

npm --prefix apps/site ci
npm --prefix apps/site run build
echo "Local verification passed."
