#!/bin/sh

set -eu

static_only=false
if [ "${1:-}" = "--static-only" ]; then
  static_only=true
  shift
fi
app=${1:-}
if [ -z "$app" ]; then
  echo "Usage: $0 [--static-only] /path/to/Dakia.app" >&2
  exit 2
fi

executable="$app/Contents/MacOS/dakia-desktop"
runtime="$app/Contents/Frameworks/libonnxruntime.1.23.2.dylib"
notice_policy=${DAKIA_RELEASE_NOTICE_POLICY:-current}

case "$notice_policy" in
  current|legacy-pre-0.2.12) ;;
  *)
    echo "Unsupported release notice policy: $notice_policy" >&2
    exit 2
    ;;
esac

require_sha256() {
  expected=$1
  resource=$2
  actual=$(shasum -a 256 "$resource" | awk '{print $1}')
  if [ "$actual" != "$expected" ]; then
    echo "Packaged notice does not match its audited source copy: $resource" >&2
    exit 1
  fi
}

for packaged_resource in \
  "$runtime" \
  "$app/Contents/Resources/resources/email-classifier-v2/MANIFEST.json" \
  "$app/Contents/Resources/resources/email-classifier-v2/model.onnx" \
  "$app/Contents/Resources/resources/email-classifier-v2/tokenizer.json"; do
  if [ ! -s "$packaged_resource" ]; then
    echo "Missing packaged release resource: $packaged_resource" >&2
    exit 1
  fi
done
if [ ! -x "$executable" ]; then
  echo "Missing packaged Dakia executable: $executable" >&2
  exit 1
fi

if [ "$notice_policy" = "current" ]; then
  for packaged_resource in \
    "$app/Contents/Resources/THIRD_PARTY_NOTICES.md" \
    "$app/Contents/Resources/licenses/Apache-2.0.txt" \
    "$app/Contents/Resources/licenses/MPL-2.0.txt" \
    "$app/Contents/Resources/licenses/DAKIA-MPL-2.0-SOURCE-NOTICE.md" \
    "$app/Contents/Resources/licenses/mmBERT-small-MIT-NOTICE.txt" \
    "$app/Contents/Resources/licenses/ONNX-Runtime-1.23.2-LICENSE.txt" \
    "$app/Contents/Resources/licenses/ONNX-Runtime-1.23.2-ThirdPartyNotices.txt"; do
    if [ ! -s "$packaged_resource" ]; then
      echo "Missing packaged release resource: $packaged_resource" >&2
      exit 1
    fi
  done
  require_sha256 cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30 \
    "$app/Contents/Resources/licenses/Apache-2.0.txt"
  require_sha256 3f3d9e0024b1921b067d6f7f88deb4a60cbe7a78e76c64e3f1d7fc3b779b9d04 \
    "$app/Contents/Resources/licenses/MPL-2.0.txt"
  require_sha256 eef5d343ef610b25bee312f39d2e8657cc667b510e8811b8e958f444eb9faee8 \
    "$app/Contents/Resources/licenses/DAKIA-MPL-2.0-SOURCE-NOTICE.md"
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
    ! grep -Fq "Dakia is licensed under the Mozilla Public License 2.0" \
      "$app/Contents/Resources/THIRD_PARTY_NOTICES.md" ||
    ! grep -Fq "https://github.com/DakiaMail/dakia-desktop" \
      "$app/Contents/Resources/licenses/DAKIA-MPL-2.0-SOURCE-NOTICE.md" ||
    ! grep -Fq "THIRD PARTY SOFTWARE NOTICES AND INFORMATION" \
      "$app/Contents/Resources/licenses/ONNX-Runtime-1.23.2-ThirdPartyNotices.txt"; then
    echo "Packaged third-party notices are incomplete." >&2
    exit 1
  fi
fi

if [ "$static_only" = true ]; then
  echo "Packaged Dakia static app/resource/legal verification passed: $app"
  exit 0
fi

smoke_root=$(mktemp -d "${TMPDIR:-/tmp}/dakia-release-smoke.XXXXXX")
output="$smoke_root/output.log"
trap 'rm -rf "$smoke_root"' EXIT HUP INT TERM
mkdir -p "$smoke_root/data"

DAKIA_RELEASE_SMOKE_TEST=1 \
DAKIA_RELEASE_SMOKE_DATA_DIR="$smoke_root/data" \
RUST_BACKTRACE=1 \
"$executable" >"$output" 2>&1 &
pid=$!

seconds=0
while kill -0 "$pid" 2>/dev/null; do
  if [ "$seconds" -ge 30 ]; then
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    echo "Packaged Dakia startup smoke test timed out." >&2
    cat "$output" >&2
    exit 1
  fi
  sleep 1
  seconds=$((seconds + 1))
done

set +e
wait "$pid"
status=$?
set -e

if [ "$status" -ne 0 ]; then
  echo "Packaged Dakia startup smoke test exited with status $status." >&2
  cat "$output" >&2
  exit 1
fi
if ! grep -Fq "DAKIA_RELEASE_SMOKE_TEST_OK" "$output"; then
  echo "Packaged Dakia did not complete native startup initialization." >&2
  cat "$output" >&2
  exit 1
fi

echo "Packaged Dakia startup smoke test passed: $app"
