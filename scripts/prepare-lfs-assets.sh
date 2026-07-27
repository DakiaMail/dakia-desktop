#!/bin/sh

set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
assets="
apps/desktop/src-tauri/resources/email-classifier-v2/model.onnx
apps/desktop/src-tauri/resources/email-classifier-v2/tokenizer.json
"

is_lfs_pointer() {
  [ "$(dd if="$1" bs=7 count=1 2>/dev/null)" = "version" ]
}

needs_checkout=false
for relative_path in $assets; do
  if [ ! -f "$repo_root/$relative_path" ] ||
    is_lfs_pointer "$repo_root/$relative_path"; then
    needs_checkout=true
    break
  fi
done

cd "$repo_root"
if [ "$needs_checkout" = true ]; then
  if ! command -v git-lfs >/dev/null 2>&1; then
    echo "Dakia's classifier assets require Git LFS. Install git-lfs and run npm run prepare:desktop-assets." >&2
    exit 1
  fi

  # Some restricted worktrees cannot update the shared Git index even after the
  # working-tree files were successfully materialized, so verify the files below
  # instead of relying only on this command's exit status.
  git lfs checkout -- $assets || true
fi

for relative_path in $assets; do
  if [ ! -f "$relative_path" ] || is_lfs_pointer "$relative_path"; then
    echo "Could not materialize $relative_path from Git LFS." >&2
    echo "Run 'git lfs pull' and then 'npm run prepare:desktop-assets'." >&2
    exit 1
  fi
done

node "$repo_root/scripts/verify-classifier-assets.mjs"
