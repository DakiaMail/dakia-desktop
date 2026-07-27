#!/bin/sh

set -eu

if [ "$(uname -s)" != "Darwin" ]; then
  echo "macOS updater packaging must run on macOS." >&2
  exit 1
fi

if [ "$#" -ne 3 ]; then
  echo "Usage: $0 <aarch64|x86_64> <version> /path/to/output-dir" >&2
  exit 2
fi

arch=$1
version=$2
output_dir=$3
root_dir=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)

case "$arch" in
  aarch64)
    if [ -d "$root_dir/target/aarch64-apple-darwin/release/bundle/macos" ]; then
      bundle_root="$root_dir/target/aarch64-apple-darwin/release/bundle/macos"
    else
      bundle_root="$root_dir/target/release/bundle/macos"
    fi
    ;;
  x86_64)
    bundle_root="$root_dir/target/x86_64-apple-darwin/release/bundle/macos"
    ;;
  *)
    echo "Unsupported architecture '$arch'." >&2
    exit 2
    ;;
esac

app="$bundle_root/Dakia.app"
archive="$output_dir/Dakia-$arch.app.tar.gz"
signature="$archive.sig"

require_sha256() {
  expected=$1
  resource=$2
  actual=$(shasum -a 256 "$resource" | awk '{print $1}')
  if [ "$actual" != "$expected" ]; then
    echo "Bundled notice does not match its audited source copy: $resource" >&2
    exit 1
  fi
}

if [ ! -d "$app" ]; then
  echo "Missing app bundle: $app" >&2
  exit 1
fi
for bundled_resource in \
  "$app/Contents/MacOS/dakia" \
  "$app/Contents/Frameworks/libonnxruntime.1.23.2.dylib" \
  "$app/Contents/Resources/THIRD_PARTY_NOTICES.md" \
  "$app/Contents/Resources/licenses/Apache-2.0.txt" \
  "$app/Contents/Resources/licenses/MPL-2.0.txt" \
  "$app/Contents/Resources/licenses/mmBERT-small-MIT-NOTICE.txt" \
  "$app/Contents/Resources/licenses/ONNX-Runtime-1.23.2-LICENSE.txt" \
  "$app/Contents/Resources/licenses/ONNX-Runtime-1.23.2-ThirdPartyNotices.txt" \
  "$app/Contents/Resources/resources/email-classifier-v2/MANIFEST.json" \
  "$app/Contents/Resources/resources/email-classifier-v2/model.onnx" \
  "$app/Contents/Resources/resources/email-classifier-v2/tokenizer.json"; do
  if [ ! -s "$bundled_resource" ]; then
    echo "Missing bundled updater resource: $bundled_resource" >&2
    exit 1
  fi
done
require_sha256 cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30 \
  "$app/Contents/Resources/licenses/Apache-2.0.txt"
require_sha256 3f3d9e0024b1921b067d6f7f88deb4a60cbe7a78e76c64e3f1d7fc3b779b9d04 \
  "$app/Contents/Resources/licenses/MPL-2.0.txt"
require_sha256 37bd7f5f301ccab826b60d0f225137e228505d3d3e0fb68bd33a8cdb33883e62 \
  "$app/Contents/Resources/licenses/mmBERT-small-MIT-NOTICE.txt"
require_sha256 2f07c72751aed99790b8a4869cf2311df85a860b22ded05fa22803587a48922c \
  "$app/Contents/Resources/licenses/ONNX-Runtime-1.23.2-LICENSE.txt"
require_sha256 e9e90971a8e75a9a8ac0c6412e29c1202d079998389915aa485f46c816c3b4cc \
  "$app/Contents/Resources/licenses/ONNX-Runtime-1.23.2-ThirdPartyNotices.txt"
if ! grep -Fq "72de7110305b5e1d98d26aa0578482a230739c0c" \
  "$app/Contents/Resources/THIRD_PARTY_NOTICES.md" ||
  ! grep -Fq "jhu-clsp/mmBERT-small" \
    "$app/Contents/Resources/THIRD_PARTY_NOTICES.md" ||
  ! grep -Fq "ONNX Runtime 1.23.2" \
    "$app/Contents/Resources/THIRD_PARTY_NOTICES.md" ||
  ! grep -Fq "THIRD PARTY SOFTWARE NOTICES AND INFORMATION" \
    "$app/Contents/Resources/licenses/ONNX-Runtime-1.23.2-ThirdPartyNotices.txt"; then
  echo "Bundled third-party notices are incomplete." >&2
  exit 1
fi
if [ -z "${TAURI_SIGNING_PRIVATE_KEY:-}" ]; then
  echo "TAURI_SIGNING_PRIVATE_KEY is required." >&2
  exit 1
fi
if [ -z "${TAURI_SIGNING_PRIVATE_KEY_PASSWORD+x}" ]; then
  echo "TAURI_SIGNING_PRIVATE_KEY_PASSWORD must be set, even when empty." >&2
  exit 1
fi

codesign --verify --deep --strict --verbose=2 "$app"
spctl --assess --type execute --verbose=2 "$app"
xcrun stapler validate "$app"

actual_version=$(/usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" \
  "$app/Contents/Info.plist")
if [ "$actual_version" != "$version" ]; then
  echo "App version '$actual_version' does not match '$version'." >&2
  exit 1
fi

mkdir -p "$output_dir"
rm -f "$archive" "$signature"
COPYFILE_DISABLE=1 tar -czf "$archive" -C "$bundle_root" Dakia.app

verify_dir=$(mktemp -d "${TMPDIR:-/tmp}/dakia-updater-verify.XXXXXX")
trap 'rm -rf "$verify_dir"' EXIT HUP INT TERM
tar -xzf "$archive" -C "$verify_dir"
codesign --verify --deep --strict --verbose=2 "$verify_dir/Dakia.app"
spctl --assess --type execute --verbose=2 "$verify_dir/Dakia.app"
xcrun stapler validate "$verify_dir/Dakia.app"

(cd "$root_dir" && npm run tauri signer sign -- "$archive")

if [ ! -s "$archive" ] || [ ! -s "$signature" ]; then
  echo "Updater archive or signature is missing." >&2
  exit 1
fi

rm -rf "$verify_dir"
trap - EXIT HUP INT TERM
echo "Packaged signed updater: $archive"
