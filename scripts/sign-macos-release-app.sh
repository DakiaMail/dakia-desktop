#!/bin/sh

set -eu

app=${1:-}
identity=${2:-${APPLE_SIGNING_IDENTITY:-}}
if [ -z "$app" ] || [ -z "$identity" ]; then
  echo "Usage: $0 /path/to/Dakia.app 'Developer ID Application: …'" >&2
  exit 2
fi
if [ ! -d "$app" ]; then
  echo "Missing app bundle: $app" >&2
  exit 1
fi

framework="$app/Contents/Frameworks/libonnxruntime.1.23.2.dylib"
sidecar="$app/Contents/MacOS/dakia"
executable="$app/Contents/MacOS/dakia-desktop"

for nested in "$framework" "$sidecar" "$executable"; do
  if [ ! -f "$nested" ]; then
    echo "Missing nested executable: $nested" >&2
    exit 1
  fi
  codesign --force --options runtime --timestamp --sign "$identity" "$nested"
done

codesign --force --options runtime --timestamp --sign "$identity" "$app"
codesign --verify --deep --strict --verbose=2 "$app"

echo "Signed and verified final app bundle: $app"
