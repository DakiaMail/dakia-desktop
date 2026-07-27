#!/bin/sh

set -eu

if [ "$(uname -s)" != "Darwin" ]; then
  echo "DMG rebuild must run on macOS." >&2
  exit 1
fi

if [ "$#" -ne 2 ]; then
  echo "Usage: $0 <version> /path/to/output.dmg" >&2
  exit 2
fi

version=$1
output_dmg=$2
identity=${APPLE_SIGNING_IDENTITY:-}
notary_profile=${APPLE_NOTARY_PROFILE:-dakia-notary}

if [ -z "$identity" ]; then
  echo "APPLE_SIGNING_IDENTITY is required." >&2
  exit 1
fi

root_dir=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)

bundle_root="$root_dir/target/aarch64-apple-darwin/release/bundle"

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

expected_name="Dakia_${version}_aarch64.dmg"
actual_name=$(basename "$output_dmg")
if [ "$actual_name" != "$expected_name" ]; then
  echo "Rebuilt DMG at $output_dmg"
else
  echo "Rebuilt notarized DMG: $output_dmg"
fi
