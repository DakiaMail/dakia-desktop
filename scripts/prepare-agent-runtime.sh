#!/bin/sh

set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
agent_source=${DAKIA_AGENT_PACKAGE_SOURCE:-"$repo_root/packages/dakia-agent"}
destination=${DAKIA_AGENT_RUNTIME_DESTINATION:-"$repo_root/apps/desktop/src-tauri/resources/agent"}
node_version=22.23.2
minimum_macos_version=11.0

verify_destination=
if [ "${1:-}" = "--verify" ]; then
  verify_destination=${2:-$destination}
elif [ "$#" -gt 0 ]; then
  echo "Usage: $0 [--verify [runtime-directory]]" >&2
  exit 2
fi

platform=${DAKIA_AGENT_RUNTIME_PLATFORM:-${TAURI_ENV_PLATFORM:-$(uname -s | tr '[:upper:]' '[:lower:]')}}
architecture=${DAKIA_AGENT_RUNTIME_ARCH:-${TAURI_ENV_ARCH:-$(uname -m)}}
case "$platform" in
  macos | darwin) platform=darwin ;;
  linux) platform=linux ;;
  windows | mingw* | msys*) platform=win ;;
  *)
    echo "The Dakia agent runtime is not supported on platform: $platform" >&2
    exit 1
    ;;
esac
case "$architecture" in
  aarch64 | arm64) architecture=arm64 ;;
  x86_64 | amd64 | x64) architecture=x64 ;;
  *)
    echo "The Dakia agent runtime is not supported on architecture: $architecture" >&2
    exit 1
    ;;
esac

if [ -n "$verify_destination" ]; then
  node "$repo_root/scripts/prepare-agent-runtime.mjs" verify "$verify_destination" "$platform" "$architecture"
  exit
fi

case "$platform-$architecture" in
  darwin-arm64)
    archive_name="node-v$node_version-darwin-arm64.tar.gz"
    archive_sha256=61130f394c1630d211dd50aecc4353d379480f36d3ac913cd85dbba1aed585c6
    ;;
  darwin-x64)
    archive_name="node-v$node_version-darwin-x64.tar.gz"
    archive_sha256=58e99022c2ff89395576cc7fd4d98cea24bb68081475d5f88b801ee8729fb026
    ;;
  linux-arm64)
    archive_name="node-v$node_version-linux-arm64.tar.gz"
    archive_sha256=013b59cfd2819703a6f4a14ab891fc46fc2a4e3f5bcd92de3fb4929b43e35b30
    ;;
  linux-x64)
    archive_name="node-v$node_version-linux-x64.tar.gz"
    archive_sha256=b294a556e639d64338823920e5866c21c02741742d2e1529ee1a225c1ec9252a
    ;;
  win-arm64)
    archive_name="node-v$node_version-win-arm64.zip"
    archive_sha256=fec025a6da31757e3b6af84c5a1628e9d38442ca99a2161091d78f2fcfa35ef3
    ;;
  win-x64)
    archive_name="node-v$node_version-win-x64.zip"
    archive_sha256=1177b4137ba5adaa56354ae40f1080c7450e8ae09cecb47da459d1c52ac99f97
    ;;
  *)
    echo "No pinned Node.js runtime exists for $platform-$architecture." >&2
    exit 1
    ;;
esac

if [ "${DAKIA_AGENT_RUNTIME_TESTING:-}" = 1 ]; then
  archive_sha256=${DAKIA_AGENT_NODE_ARCHIVE_SHA256:-$archive_sha256}
elif [ -n "${DAKIA_AGENT_NODE_ARCHIVE_SHA256:-}" ]; then
  echo "A Node.js checksum override is allowed only in runtime packaging tests." >&2
  exit 1
fi

for required in "$agent_source/package.json" "$agent_source/package-lock.json" "$agent_source/dist/cli.js"; do
  if [ ! -f "$required" ]; then
    echo "Missing agent packaging input: $required" >&2
    echo "Run npm run build:agent after installing locked dependencies." >&2
    exit 1
  fi
done
for command in node npm curl tar; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "Required agent packaging command is missing: $command" >&2
    exit 1
  fi
done
if [ "$platform" = win ] && ! command -v unzip >/dev/null 2>&1; then
  echo "Required agent packaging command is missing: unzip" >&2
  exit 1
fi

archive_matches_checksum() {
  [ -f "$1" ] &&
    [ "$(node "$repo_root/scripts/prepare-agent-runtime.mjs" sha256 "$1")" = "$archive_sha256" ]
}

destination_parent=$(dirname "$destination")
mkdir -p "$destination_parent"
stage=$(mktemp -d "$destination_parent/.agent-runtime.XXXXXX")
cleanup() {
  rm -rf "$stage"
}
trap cleanup EXIT HUP INT TERM

bundle="$stage/bundle"
npm_stage="$stage/npm"
archive="$stage/$archive_name"
extracted="$stage/extracted"
mkdir -p "$bundle/dist" "$bundle/runtime" "$bundle/licenses/node" "$bundle/licenses/pi" "$bundle/licenses/npm-supplemental" "$npm_stage" "$extracted"

cp -R "$agent_source/dist/." "$bundle/dist/"
cp "$agent_source/package.json" "$bundle/package.json"
cp "$agent_source/package-lock.json" "$bundle/package-lock.json"
cp "$agent_source/package.json" "$agent_source/package-lock.json" "$npm_stage/"
cat >"$bundle/licenses/pi/LICENSE" <<'LICENSE'
MIT License

Copyright (c) 2025 Mario Zechner

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
LICENSE
cp "$repo_root/apps/desktop/src-tauri/resources/licenses/Apache-2.0.txt" \
  "$bundle/licenses/npm-supplemental/Apache-2.0.txt"

(
  cd "$npm_stage"
  npm ci --omit=dev --ignore-scripts --no-audit --no-fund
  npm ls --omit=dev >/dev/null
)
if [ ! -d "$npm_stage/node_modules" ]; then
  echo "Locked agent dependency installation produced no node_modules directory." >&2
  exit 1
fi
if [ -d "$npm_stage/node_modules/proxy-agent-negotiate" ]; then
  proxy_agent_license="$npm_stage/node_modules/https-proxy-agent/LICENSE"
  if [ ! -f "$proxy_agent_license" ]; then
    echo "The locked proxy-agents dependency tree is missing its authoritative MIT notice." >&2
    exit 1
  fi
  cp "$proxy_agent_license" \
    "$bundle/licenses/npm-supplemental/proxy-agent-negotiate-MIT.txt"
fi
cp -R "$npm_stage/node_modules" "$bundle/node_modules"
find "$bundle/node_modules" -type d -name .bin -prune -exec rm -rf {} +
node "$repo_root/scripts/prepare-agent-runtime.mjs" inventory "$bundle"

if [ -n "${DAKIA_AGENT_NODE_ARCHIVE:-}" ]; then
  cp "$DAKIA_AGENT_NODE_ARCHIVE" "$archive"
else
  cache_dir=${DAKIA_AGENT_RUNTIME_CACHE_DIR:-"$repo_root/target/agent-runtime-cache"}
  cached_archive="$cache_dir/$archive_name"
  mkdir -p "$cache_dir"
  if archive_matches_checksum "$cached_archive"; then
    cp "$cached_archive" "$archive"
  else
    rm -f "$cached_archive"
    curl --fail --location --retry 3 --silent --show-error \
      "https://nodejs.org/download/release/v$node_version/$archive_name" \
      --output "$archive"
    if ! archive_matches_checksum "$archive"; then
      echo "Downloaded Node.js runtime checksum does not match the pinned release." >&2
      exit 1
    fi
    cp "$archive" "$cached_archive"
  fi
fi
if ! archive_matches_checksum "$archive"; then
  echo "Node.js runtime archive checksum does not match the pinned release." >&2
  exit 1
fi

if [ "$platform" = win ]; then
  unzip -q "$archive" -d "$extracted"
  node_root="$extracted/node-v$node_version-win-$architecture"
  node_binary="$node_root/node.exe"
  bundled_node="$bundle/runtime/node.exe"
else
  tar -xzf "$archive" -C "$extracted"
  node_root="$extracted/node-v$node_version-$platform-$architecture"
  node_binary="$node_root/bin/node"
  bundled_node="$bundle/runtime/node"
fi
if [ ! -f "$node_binary" ] || [ ! -f "$node_root/LICENSE" ]; then
  echo "Official Node.js archive is missing its runtime or license." >&2
  exit 1
fi
cp "$node_binary" "$bundled_node"
cp "$node_root/LICENSE" "$bundle/licenses/node/LICENSE"
chmod 755 "$bundled_node"

if [ "$platform" = darwin ]; then
  for command in file lipo otool; do
    if ! command -v "$command" >/dev/null 2>&1; then
      echo "Required macOS agent runtime verifier is missing: $command" >&2
      exit 1
    fi
  done
  expected_arch=$architecture
  [ "$expected_arch" = x64 ] && expected_arch=x86_64
  macos_version_is_compatible() {
    awk -v actual="$1" -v maximum="$minimum_macos_version" 'BEGIN {
      split(actual, a, "."); split(maximum, m, ".");
      if ((a[1] + 0) < (m[1] + 0)) exit 0;
      if ((a[1] + 0) > (m[1] + 0)) exit 1;
      exit ((a[2] + 0) <= (m[2] + 0)) ? 0 : 1;
    }'
  }
  actual_arch=$(lipo -archs "$bundled_node")
  if [ "$actual_arch" != "$expected_arch" ]; then
    echo "Bundled Node.js architecture is $actual_arch, expected exactly $expected_arch." >&2
    exit 1
  fi
  runtime_minos=$(otool -l "$bundled_node" | awk '/ minos / { found=1; print $2; exit } / version / { candidate=$2 } END { if (!found && candidate != "") print candidate }')
  if [ -z "$runtime_minos" ]; then
    echo "Cannot determine the bundled Node.js minimum macOS version." >&2
    exit 1
  fi
  if ! macos_version_is_compatible "$runtime_minos"; then
    echo "Bundled Node.js requires macOS $runtime_minos, newer than Dakia's $minimum_macos_version minimum." >&2
    exit 1
  fi
  find "$bundle/node_modules" -type f -name '*.node' -print | while IFS= read -r native_module; do
    if file -b "$native_module" | grep -q 'Mach-O'; then
      module_archs=$(lipo -archs "$native_module")
      if [ -z "$module_archs" ]; then
        echo "Cannot inspect native agent dependency architecture: $native_module" >&2
        exit 1
      fi
      module_has_target=false
      for module_arch in $module_archs; do
        [ "$module_arch" = "$expected_arch" ] && module_has_target=true
      done
      case "$native_module" in
        *darwin_arm64* | *darwin-arm64* | *darwin-universal*)
          if [ "$expected_arch" = arm64 ] && [ "$module_has_target" != true ]; then
            echo "Target-selected native agent dependency lacks arm64: $native_module ($module_archs)" >&2
            exit 1
          fi
          ;;
        *darwin_x64* | *darwin-x64*)
          if [ "$expected_arch" = x86_64 ] && [ "$module_has_target" != true ]; then
            echo "Target-selected native agent dependency lacks x86_64: $native_module ($module_archs)" >&2
            exit 1
          fi
          ;;
      esac
      if [ "$module_has_target" = true ]; then
        module_minos=$(otool -l -arch "$expected_arch" "$native_module" | awk '/ minos / { found=1; print $2; exit } / version / { candidate=$2 } END { if (!found && candidate != "") print candidate }')
        if [ -z "$module_minos" ]; then
          echo "Cannot determine native agent dependency minimum macOS version: $native_module" >&2
          exit 1
        fi
        if ! macos_version_is_compatible "$module_minos"; then
          echo "Native agent dependency requires macOS $module_minos, newer than Dakia's $minimum_macos_version minimum: $native_module" >&2
          exit 1
        fi
      fi
    fi
  done
fi

runtime_version=$(
  PATH=/usr/bin:/bin "$bundled_node" --version
)
if [ "$runtime_version" != "v$node_version" ]; then
  echo "Bundled Node.js reported $runtime_version, expected v$node_version." >&2
  exit 1
fi

metadata="$stage/node-metadata.json"
node -e '
  const { writeFileSync } = require("node:fs");
  const [path, version, platform, arch, archive, sha256, minMac] = process.argv.slice(1);
  writeFileSync(path, JSON.stringify({ version, platform, architecture: arch, archive, archiveSha256: sha256, minimumMacOS: minMac || null }) + "\n");
' "$metadata" "$node_version" "$platform" "$architecture" "$archive_name" "$archive_sha256" "$([ "$platform" = darwin ] && printf '%s' "$minimum_macos_version" || true)"
node "$repo_root/scripts/prepare-agent-runtime.mjs" build "$bundle" "$metadata"
node "$repo_root/scripts/prepare-agent-runtime.mjs" verify "$bundle"

if [ -e "$destination" ] || [ -L "$destination" ]; then
  rm -rf "$destination"
fi
mv "$bundle" "$destination"
node "$repo_root/scripts/prepare-agent-runtime.mjs" verify "$destination"
echo "Prepared self-contained Dakia agent runtime: $destination"
