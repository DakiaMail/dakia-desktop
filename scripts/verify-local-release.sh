#!/usr/bin/env bash
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root_dir"

# This is the release-Mac gate, not a contributor-only test. Using the actual
# Developer ID identity keeps the app and ONNX dylib on one Apple Team ID.
# shellcheck source=local-release-env.sh
source "$root_dir/scripts/local-release-env.sh"
dakia_require_google_oauth_environment
dakia_require_signing_environment

npm run setup:worktree
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
npm run typecheck
npm run format:check
NODE_OPTIONS="--localstorage-file=/private/tmp/dakia-release-vitest-localstorage" \
  npm run test -- --maxWorkers=1
npm run test:release-scripts
npm run build:web
# Do not let the OAuth secret reach npm or Tauri's configured prebuild command.
# Only the direct Tauri compiler process and its Rust compilation children need
# the compile-time OAuth variables.
release_tauri_config="$(mktemp "${TMPDIR:-/tmp}/dakia-release-tauri-config.XXXXXX")"
printf '%s\n' '{"build":{"beforeBuildCommand":""}}' >"$release_tauri_config"
dakia_prepare_google_oauth_compiler_environment
trap 'dakia_clear_google_oauth_compiler_environment; rm -f "$release_tauri_config"' EXIT HUP INT TERM

APPLE_SIGNING_IDENTITY="$APPLE_SIGNING_IDENTITY" \
ORT_LIB_LOCATION="$root_dir/apps/desktop/src-tauri/frameworks" \
ORT_PREFER_DYNAMIC_LINK=1 \
  "$root_dir/node_modules/.bin/tauri" build --bundles app \
    --config apps/desktop/src-tauri/tauri.conf.json \
    --config apps/desktop/src-tauri/tauri.ci.conf.json \
    --config "$release_tauri_config"
"$root_dir/scripts/verify-macos-release-app.sh" \
  "$root_dir/target/release/bundle/macos/Dakia.app"

echo "Local verification passed."
