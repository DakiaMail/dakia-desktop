#!/bin/sh

set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
profile=${1:-release}
arch=${TAURI_ENV_ARCH:-$(rustc -vV | sed -n 's/^host: \([^-]*\)-.*/\1/p')}
platform=${TAURI_ENV_PLATFORM:-$(uname -s | tr '[:upper:]' '[:lower:]')}

case "$platform" in
  macos | darwin)
    target="${arch}-apple-darwin"
    ;;
  linux)
    target="${arch}-unknown-linux-gnu"
    ;;
  *)
    echo "Bundling the Dakia CLI is not supported for platform: $platform" >&2
    exit 1
    ;;
esac

case "$profile" in
  debug)
    cargo_profile=debug
    profile_args=
    ;;
  release)
    cargo_profile=release
    profile_args=--release
    ;;
  *)
    echo "Expected build profile 'debug' or 'release', got: $profile" >&2
    exit 1
    ;;
esac

cd "$repo_root"
cargo build -p dakia-cli --bin dakia --target "$target" $profile_args

destination="$repo_root/apps/desktop/src-tauri/binaries/dakia-$target"
mkdir -p "$(dirname "$destination")"
cp "$repo_root/target/$target/$cargo_profile/dakia" "$destination"
chmod 755 "$destination"

if [ "$platform" = "macos" ] || [ "$platform" = "darwin" ]; then
  # The sidecar lives in Contents/MacOS in a packaged app and next to the
  # src-tauri framework directory during development. Its ONNX Runtime load
  # command uses @rpath, so give both layouts the same relative lookup path.
  install_name_tool -add_rpath "@executable_path/../Frameworks" "$destination"
fi
