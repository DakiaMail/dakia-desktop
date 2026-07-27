#!/usr/bin/env bash
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root_dir"

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

echo "Local verification passed."
