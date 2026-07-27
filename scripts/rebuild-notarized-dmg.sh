#!/bin/sh

set -eu

if [ "$(uname -s)" != "Darwin" ]; then
  echo "DMG rebuild must run on macOS." >&2
  exit 1
fi

if [ "$#" -ne 3 ]; then
  echo "Usage: $0 <aarch64|x64> <version> /path/to/output.dmg" >&2
  exit 2
fi

arch=$1
version=$2
output_dmg=$3
identity=${APPLE_SIGNING_IDENTITY:-}
notary_profile=${APPLE_NOTARY_PROFILE:-dakia-notary}

if [ -z "$identity" ]; then
  echo "APPLE_SIGNING_IDENTITY is required." >&2
  exit 1
fi

root_dir=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)

case "$arch" in
  aarch64)
    if [ -d "$root_dir/target/aarch64-apple-darwin/release/bundle" ]; then
      bundle_root="$root_dir/target/aarch64-apple-darwin/release/bundle"
    else
      bundle_root="$root_dir/target/release/bundle"
    fi
    ;;
  x64)
    bundle_root="$root_dir/target/x86_64-apple-darwin/release/bundle"
    ;;
  *)
    echo "Unsupported architecture '$arch'. Use 'aarch64' or 'x64'." >&2
    exit 2
    ;;
esac

bundle_script="$bundle_root/dmg/bundle_dmg.sh"
app_bundle_dir="$bundle_root/macos"
app_bundle="$app_bundle_dir/Dakia.app"

if [ ! -x "$bundle_script" ]; then
  echo "Missing DMG bundle script: $bundle_script" >&2
  exit 1
fi

if [ ! -d "$app_bundle" ]; then
  echo "Missing stapled app bundle: $app_bundle" >&2
  exit 1
fi

mkdir -p "$(dirname "$output_dmg")"

staging_dir=$(mktemp -d "${TMPDIR:-/tmp}/dakia-dmg.XXXXXX")
trap 'rm -rf "$staging_dir"' EXIT HUP INT TERM
ditto "$app_bundle" "$staging_dir/Dakia.app"

"$bundle_script" \
  --volname Dakia \
  --window-size 660 420 \
  --icon-size 160 \
  --icon "Dakia.app" 160 180 \
  --hide-extension "Dakia.app" \
  --app-drop-link 500 180 \
  --codesign "$identity" \
  --notarize "$notary_profile" \
  "$output_dmg" \
  "$staging_dir"

expected_name="Dakia_${version}_${arch}.dmg"
actual_name=$(basename "$output_dmg")
if [ "$actual_name" != "$expected_name" ]; then
  echo "Rebuilt DMG at $output_dmg"
else
  echo "Rebuilt notarized DMG: $output_dmg"
fi
