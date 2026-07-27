#!/bin/sh

set -eu

if [ "$(uname -s)" != "Darwin" ]; then
  exit 0
fi

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
framework_dir="$repo_root/apps/desktop/src-tauri/frameworks"
runtime="$framework_dir/libonnxruntime.1.23.2.dylib"
runtime_link="$framework_dir/libonnxruntime.dylib"
runtime_sha256=6fee21a0dbcaa98fe082cb4f7ed07ec5def439df36198f47b61dc205e7d2a1fa
archive_sha256=49ae8e3a66ccb18d98ad3fe7f5906b6d7887df8a5edd40f49eb2b14e20885809

runtime_is_valid() {
  [ -f "$1" ] &&
    [ "$(shasum -a 256 "$1" | awk '{print $1}')" = "$runtime_sha256" ]
}

ensure_runtime_link() {
  ln -sf "$(basename "$runtime")" "$runtime_link"
}

if runtime_is_valid "$runtime"; then
  ensure_runtime_link
  exit 0
fi

mkdir -p "$framework_dir"

# Git worktrees share LFS objects but not ignored framework files. Reuse a
# verified runtime from another Dakia worktree with an APFS clone copy when
# possible, avoiding another download and almost all additional disk usage.
git -C "$repo_root" worktree list --porcelain |
  sed -n 's/^worktree //p' |
  while IFS= read -r worktree; do
    candidate="$worktree/apps/desktop/src-tauri/frameworks/libonnxruntime.1.23.2.dylib"
    if [ "$candidate" != "$runtime" ] && runtime_is_valid "$candidate"; then
      cp -c "$candidate" "$runtime" 2>/dev/null || cp "$candidate" "$runtime"
      exit 0
    fi
  done

if runtime_is_valid "$runtime"; then
  ensure_runtime_link
  exit 0
fi

archive=$(mktemp "${TMPDIR:-/tmp}/dakia-onnxruntime.XXXXXX.tgz")
extract_dir=$(mktemp -d "${TMPDIR:-/tmp}/dakia-onnxruntime.XXXXXX")
cleanup() {
  rm -f "$archive"
  rm -rf "$extract_dir"
}
trap cleanup EXIT HUP INT TERM

curl --fail --location --retry 3 --silent --show-error \
  --output "$archive" \
  "https://github.com/microsoft/onnxruntime/releases/download/v1.23.2/onnxruntime-osx-universal2-1.23.2.tgz"
echo "$archive_sha256  $archive" |
  shasum -a 256 --check

tar -xzf "$archive" -C "$extract_dir" --strip-components=2
prepared_runtime="$framework_dir/.libonnxruntime.1.23.2.dylib.tmp.$$"
cp "$extract_dir/lib/libonnxruntime.1.23.2.dylib" "$prepared_runtime"
mv "$prepared_runtime" "$runtime"
runtime_is_valid "$runtime"
ensure_runtime_link
