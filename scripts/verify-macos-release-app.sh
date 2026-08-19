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
if [ ! -d "$app" ] || [ -L "$app" ]; then
  echo "Missing or unsafe packaged Dakia app bundle: $app" >&2
  exit 1
fi
# Tauri rejects a macOS executable path with any symlinked ancestor. Common
# temporary roots such as /var and /tmp are symlinks to /private/...; launch
# the verifier smoke through the physical bundle path so resource resolution
# exercises the extracted app instead of falling back to Contents/MacOS.
app=$(CDPATH= cd -- "$app" && pwd -P)

executable="$app/Contents/MacOS/dakia-desktop"
cli="$app/Contents/MacOS/dakia"
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
for packaged_executable in "$executable" "$cli"; do
  if [ ! -f "$packaged_executable" ] || [ -L "$packaged_executable" ] || [ ! -x "$packaged_executable" ]; then
    echo "Missing or unsafe packaged Dakia executable: $packaged_executable" >&2
    exit 1
  fi
done

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

cli_archs=$(lipo -archs "$cli")
if [ "$cli_archs" != "arm64" ]; then
  echo "Packaged Dakia CLI is not exactly Apple Silicon arm64: $cli_archs" >&2
  exit 1
fi
if ! otool -l "$cli" | awk '
  $1 == "cmd" && $2 == "LC_RPATH" { in_rpath = 1; next }
  in_rpath && $1 == "path" {
    if ($2 == "@executable_path/../Frameworks") found = 1
    in_rpath = 0
  }
  END { exit(found ? 0 : 1) }
'; then
  echo "Packaged Dakia CLI cannot resolve the bundled ONNX Runtime framework." >&2
  exit 1
fi
codesign --verify --strict --verbose=2 "$cli"
app_team=$(
  codesign -dv --verbose=4 "$app" 2>&1 |
    sed -n 's/^TeamIdentifier=//p'
)
cli_team=$(
  codesign -dv --verbose=4 "$cli" 2>&1 |
    sed -n 's/^TeamIdentifier=//p'
)
if [ -z "$app_team" ] || [ "$cli_team" != "$app_team" ]; then
  echo "Packaged Dakia CLI TeamIdentifier does not match the outer app." >&2
  exit 1
fi

umask 077
smoke_root=$(mktemp -d "${TMPDIR:-/tmp}/dakia-release-smoke.XXXXXX")
output="$smoke_root/output.log"
trap 'rm -rf "$smoke_root"' EXIT HUP INT TERM
mkdir -p "$smoke_root/data"

version=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$app/Contents/Info.plist")
cli_contract_data="$smoke_root/cli-parse-contract"
cli_version_stdout="$smoke_root/cli-version.stdout"
cli_version_stderr="$smoke_root/cli-version.stderr"
if ! "$cli" --data-dir "$cli_contract_data" --version \
  >"$cli_version_stdout" 2>"$cli_version_stderr"; then
  echo "Packaged Dakia CLI version contract failed." >&2
  cat "$cli_version_stderr" >&2
  exit 1
fi
if [ -s "$cli_version_stderr" ]; then
  echo "Packaged Dakia CLI version contract wrote unexpected stderr output." >&2
  cat "$cli_version_stderr" >&2
  exit 1
fi
cli_version=$(cat "$cli_version_stdout")
if [ "$cli_version" != "dakia $version" ]; then
  echo "Packaged Dakia CLI version does not match the app: $cli_version (expected dakia $version)" >&2
  exit 1
fi

cli_help_stdout="$smoke_root/cli-help.stdout"
cli_help_stderr="$smoke_root/cli-help.stderr"
if ! "$cli" --data-dir "$cli_contract_data" --help \
  >"$cli_help_stdout" 2>"$cli_help_stderr"; then
  echo "Packaged Dakia CLI help contract failed." >&2
  cat "$cli_help_stderr" >&2
  exit 1
fi
if [ -s "$cli_help_stderr" ] || \
  ! grep -Fq "Search, read, and send mail from the terminal" "$cli_help_stdout" || \
  ! grep -Fq "Usage: dakia [OPTIONS] <COMMAND>" "$cli_help_stdout"; then
  echo "Packaged Dakia CLI help contract returned an unexpected schema." >&2
  cat "$cli_help_stderr" >&2
  cat "$cli_help_stdout" >&2
  exit 1
fi

cli_invalid_stdout="$smoke_root/cli-invalid.stdout"
cli_invalid_stderr="$smoke_root/cli-invalid.stderr"
if "$cli" --data-dir "$cli_contract_data" not-a-command \
  >"$cli_invalid_stdout" 2>"$cli_invalid_stderr"; then
  cli_invalid_status=0
else
  cli_invalid_status=$?
fi
if [ "$cli_invalid_status" -ne 2 ] || [ -s "$cli_invalid_stdout" ] || \
  ! grep -Fq "error: unrecognized subcommand 'not-a-command'" "$cli_invalid_stderr" || \
  ! grep -Fq "For more information, try '--help'." "$cli_invalid_stderr"; then
  echo "Packaged Dakia CLI invalid-input contract was not rejected as expected." >&2
  cat "$cli_invalid_stderr" >&2
  cat "$cli_invalid_stdout" >&2
  exit 1
fi
if [ -e "$cli_contract_data" ]; then
  echo "Packaged Dakia CLI parse-only commands unexpectedly created profile state." >&2
  exit 1
fi

cli_stdout="$smoke_root/cli-stdout.json"
cli_stderr="$smoke_root/cli-stderr.log"
if ! "$cli" --data-dir "$smoke_root/cli-data" --json account list \
  >"$cli_stdout" 2>"$cli_stderr"; then
  echo "Packaged Dakia CLI isolated account-list smoke failed." >&2
  cat "$cli_stderr" >&2
  exit 1
fi
if [ -s "$cli_stderr" ]; then
  echo "Packaged Dakia CLI wrote unexpected stderr output." >&2
  cat "$cli_stderr" >&2
  exit 1
fi
if ! node -e '
  const fs = require("node:fs");
  const value = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
  if (!Array.isArray(value) || value.length !== 0) process.exit(1);
' "$cli_stdout"; then
  echo "Packaged Dakia CLI isolated account-list smoke returned an unexpected schema." >&2
  cat "$cli_stdout" >&2
  exit 1
fi

env -u DAKIA_GOOGLE_CLIENT_ID -u DAKIA_GOOGLE_CLIENT_SECRET \
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
if ! grep -Fq "DAKIA_RELEASE_GOOGLE_OAUTH_CONFIG_OK" "$output"; then
  echo "Packaged Dakia is missing its compiled Google OAuth configuration." >&2
  cat "$output" >&2
  exit 1
fi

echo "Packaged Dakia startup smoke test passed: $app"
