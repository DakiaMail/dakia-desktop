#!/bin/sh

set -eu

dmg=${1:-}
if [ -z "$dmg" ]; then
  echo "Usage: $0 /path/to/Dakia.dmg" >&2
  exit 2
fi
if [ "$(uname -s)" != "Darwin" ]; then
  echo "macOS release DMG verification must run on macOS." >&2
  exit 1
fi
if [ ! -f "$dmg" ]; then
  echo "Missing release DMG: $dmg" >&2
  exit 1
fi

mount_dir=$(mktemp -d /private/tmp/dakia-release-dmg.XXXXXX)
mounted=0
cleanup() {
  if [ "$mounted" -eq 1 ]; then
    hdiutil detach "$mount_dir" >/dev/null 2>&1 || true
  fi
  rmdir "$mount_dir" >/dev/null 2>&1 || true
}
trap cleanup EXIT HUP INT TERM

codesign --verify --deep --strict --verbose=2 "$dmg"
spctl --assess --type open --context context:primary-signature --verbose=2 "$dmg"
xcrun stapler validate "$dmg"

hdiutil attach "$dmg" -mountpoint "$mount_dir" -nobrowse -readonly
mounted=1
app="$mount_dir/Dakia.app"
executable="$app/Contents/MacOS/dakia-desktop"
applications_link="$mount_dir/Applications"

if [ ! -L "$applications_link" ]; then
  echo "Release DMG is missing the Applications shortcut." >&2
  exit 1
fi

applications_target=$(readlink "$applications_link")
if [ "$applications_target" != "/Applications" ]; then
  echo "Release DMG Applications shortcut points to '$applications_target', expected '/Applications'." >&2
  exit 1
fi

codesign --verify --deep --strict --verbose=2 "$app"
spctl --assess --type execute --verbose=2 "$app"
xcrun stapler validate "$app"

host_arch=$(uname -m)
if file "$executable" | grep -q "$host_arch"; then
  "$(dirname "$0")/verify-macos-release-app.sh" "$app"
else
  "$(dirname "$0")/verify-macos-release-app.sh" --static-only "$app"
  echo "Static app/resource/legal verification passed; skipping startup execution for non-host architecture: $(file "$executable")"
fi

hdiutil detach "$mount_dir"
mounted=0
rmdir "$mount_dir"
trap - EXIT HUP INT TERM

echo "macOS release DMG verification passed: $dmg"
