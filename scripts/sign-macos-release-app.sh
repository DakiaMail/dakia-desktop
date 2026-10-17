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
agent="$app/Contents/Resources/agent"
agent_node="$agent/runtime/node"

for nested in "$framework" "$sidecar" "$executable" "$agent_node" "$agent/MANIFEST.json"; do
  if [ ! -f "$nested" ]; then
    echo "Missing nested executable: $nested" >&2
    exit 1
  fi
done
if [ ! -d "$agent/node_modules" ]; then
  echo "Missing packaged agent dependencies: $agent/node_modules" >&2
  exit 1
fi

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
node "$repo_root/scripts/prepare-agent-runtime.mjs" verify "$agent" darwin arm64

if find "$agent" -type l -print -quit | grep . >/dev/null; then
  echo "The packaged agent runtime must not contain symbolic links." >&2
  exit 1
fi

entitlements=$(mktemp "${TMPDIR:-/tmp}/dakia-agent-entitlements.XXXXXX")
entitlements_output=$(mktemp "${TMPDIR:-/tmp}/dakia-agent-signed-entitlements.XXXXXX")
native_files=$(mktemp "${TMPDIR:-/tmp}/dakia-agent-native-files.XXXXXX")
signed_paths=$(mktemp "${TMPDIR:-/tmp}/dakia-agent-signed-paths.XXXXXX")
cleanup() {
  rm -f "$entitlements" "$entitlements_output" "$native_files" "$signed_paths"
}
trap cleanup EXIT HUP INT TERM
cat >"$entitlements" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>com.apple.security.cs.allow-jit</key>
  <true/>
  <key>com.apple.security.cs.allow-unsigned-executable-memory</key>
  <true/>
</dict>
</plist>
PLIST

# Native dependencies must be signed before the Node.js host that loads them.
# `file` keeps ordinary JavaScript, JSON, and license resources out of codesign.
find "$agent/node_modules" -type f -print >"$native_files"
while IFS= read -r native; do
  if file -b "$native" | grep -q 'Mach-O'; then
    codesign --force --options runtime --timestamp --sign "$identity" "$native"
    codesign --verify --strict --verbose=2 "$native"
    printf '%s\n' "${native#"$agent/"}" >>"$signed_paths"
  fi
done <"$native_files"

codesign --force --options runtime --timestamp --entitlements "$entitlements" --sign "$identity" "$agent_node"
codesign --verify --strict --verbose=2 "$agent_node"
printf '%s\n' runtime/node >>"$signed_paths"
codesign -d --entitlements :- "$agent_node" >"$entitlements_output" 2>&1
for entitlement in \
  com.apple.security.cs.allow-jit \
  com.apple.security.cs.allow-unsigned-executable-memory; do
  if ! grep -q "<$entitlement>" "$entitlements_output" && ! grep -q "$entitlement" "$entitlements_output"; then
    echo "Signed Node.js runtime is missing required entitlement: $entitlement" >&2
    exit 1
  fi
done

# Signing changes Mach-O bytes. Refresh the sealed runtime manifest before the
# outer app signature records every resource checksum.
node "$repo_root/scripts/prepare-agent-runtime.mjs" refresh "$agent" "$signed_paths"
node "$repo_root/scripts/prepare-agent-runtime.mjs" verify "$agent" darwin arm64

for nested in "$framework" "$sidecar" "$executable"; do
  codesign --force --options runtime --timestamp --sign "$identity" "$nested"
done

codesign --force --options runtime --timestamp --sign "$identity" "$app"
codesign --verify --deep --strict --verbose=2 "$app"

echo "Signed and verified final app bundle: $app"
