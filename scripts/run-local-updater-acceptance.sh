#!/usr/bin/env bash
set -euo pipefail

baseline_tag="${1:-}"
target_tag="${2:-}"
mode="${3:-}"
baseline_dmg="${4:-}"
target_assets="${5:-}"
evidence_root="${6:-}"
root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ ! "$baseline_tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ || \
      ! "$target_tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ || \
      ( "$mode" != "valid" && "$mode" != "tampered-archive" && \
        "$mode" != "invalid-signature" ) || \
      -z "$baseline_dmg" || -z "$target_assets" || -z "$evidence_root" ]]; then
  echo "Usage: $0 <baseline-tag> <target-tag> <valid|tampered-archive|invalid-signature> <baseline.dmg> <target-assets|-> <evidence-root>" >&2
  exit 2
fi
if [[ "$baseline_tag" == "$target_tag" || ! -s "$baseline_dmg" ]]; then
  echo "A real, older baseline DMG is required." >&2
  exit 1
fi
if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "Native updater acceptance requires macOS." >&2
  exit 1
fi

case "$(uname -m)" in
  arm64) arch="aarch64" ;;
  x86_64) arch="x86_64" ;;
  *) echo "Unsupported Mac architecture: $(uname -m)" >&2; exit 1 ;;
esac

version_is_before() {
  local candidate="$1"
  local boundary="$2"
  local candidate_major candidate_minor candidate_patch
  local boundary_major boundary_minor boundary_patch
  IFS=. read -r candidate_major candidate_minor candidate_patch <<<"$candidate"
  IFS=. read -r boundary_major boundary_minor boundary_patch <<<"$boundary"
  ((candidate_major < boundary_major)) ||
    ((candidate_major == boundary_major && candidate_minor < boundary_minor)) ||
    ((candidate_major == boundary_major && candidate_minor == boundary_minor &&
      candidate_patch < boundary_patch))
}

result_dir="$evidence_root/$arch/$mode"
if [[ -e "$result_dir/result.json" ]]; then
  echo "Refusing to overwrite existing acceptance evidence: $result_dir/result.json" >&2
  exit 1
fi
mkdir -p "$result_dir"

if [[ "${DAKIA_SKIP_FIXTURE_PUBLISH:-0}" != "1" ]]; then
  if [[ "$target_assets" == "-" ]]; then
    echo "Target assets are required when publishing the staging fixture." >&2
    exit 1
  fi
  # shellcheck source=local-release-env.sh
  source "$root_dir/scripts/local-release-env.sh"
  dakia_require_r2_environment
  "$root_dir/scripts/publish-staging-updater-fixture.sh" \
    "$target_tag" "$mode" "$target_assets"
fi

baseline_version="${baseline_tag#v}"
if version_is_before "$baseline_version" "0.2.9"; then
  DAKIA_RELEASE_NOTICE_POLICY=legacy-pre-0.2.9 \
    "$root_dir/scripts/verify-macos-release-dmg.sh" "$baseline_dmg"
else
  "$root_dir/scripts/verify-macos-release-dmg.sh" "$baseline_dmg"
fi
work_dir="$(mktemp -d "${TMPDIR:-/tmp}/dakia-updater-acceptance.XXXXXX")"
work_dir="$(cd "$work_dir" && pwd -P)"
mount_point=""
cleanup() {
  exit_status=$?
  pkill -f "$work_dir/Dakia.app/Contents/MacOS/dakia-desktop" >/dev/null 2>&1 || true
  if [[ -n "$mount_point" ]]; then
    hdiutil detach "$mount_point" >/dev/null 2>&1 || true
  fi
  if [[ "$exit_status" -ne 0 && -d "$work_dir" ]]; then
    diagnostics="$result_dir/failed-$(date -u +'%Y%m%dT%H%M%SZ')"
    mkdir -p "$diagnostics"
    for file in initialization.log app.log evidence.jsonl \
      profile-before.txt profile-after.txt; do
      [[ -e "$work_dir/$file" ]] && cp "$work_dir/$file" "$diagnostics/"
    done
    echo "Preserved failed acceptance diagnostics: $diagnostics" >&2
  fi
  rm -rf "$work_dir"
  return "$exit_status"
}
trap cleanup EXIT HUP INT TERM

mount_point="$work_dir/mount"
mkdir -p "$mount_point"
hdiutil attach "$baseline_dmg" -nobrowse -readonly -mountpoint "$mount_point"
ditto "$mount_point/Dakia.app" "$work_dir/Dakia.app"
hdiutil detach "$mount_point"
mount_point=""

app="$work_dir/Dakia.app"
executable="$app/Contents/MacOS/dakia-desktop"
data_dir="$work_dir/data"
evidence="$work_dir/evidence.jsonl"
output="$work_dir/app.log"
profile_before="$work_dir/profile-before.txt"
profile_after="$work_dir/profile-after.txt"
target_version="${target_tag#v}"

actual_baseline="$(/usr/libexec/PlistBuddy -c \
  'Print :CFBundleShortVersionString' "$app/Contents/Info.plist")"
if [[ "$actual_baseline" != "$baseline_version" ]]; then
  echo "Baseline app is $actual_baseline, expected $baseline_version." >&2
  exit 1
fi

mkdir -p "$data_dir"
open -n -W -g \
  --stdout "$work_dir/initialization.log" \
  --stderr "$work_dir/initialization.log" \
  --env DAKIA_RELEASE_SMOKE_TEST=1 \
  --env DAKIA_RELEASE_SMOKE_DATA_DIR="$data_dir" \
  "$app"
grep -Fq "DAKIA_RELEASE_SMOKE_TEST_OK" "$work_dir/initialization.log"
"$root_dir/scripts/seed-updater-acceptance-profile.sh" "$data_dir"
"$root_dir/scripts/snapshot-updater-acceptance-profile.sh" "$data_dir" |
  tee "$profile_before"
grep -q '^accounts=1 messages=2 sha256=' "$profile_before"

expect_rejection=0
expected_terminal_event="completed"
if [[ "$mode" != "valid" ]]; then
  expect_rejection=1
  expected_terminal_event="signature-rejected"
fi

open -n -g \
  --stdout "$output" \
  --stderr "$output" \
  --env DAKIA_UPDATER_ACCEPTANCE=1 \
  --env DAKIA_UPDATER_ACCEPTANCE_DATA_DIR="$data_dir" \
  --env DAKIA_UPDATER_ACCEPTANCE_EVIDENCE="$evidence" \
  --env DAKIA_UPDATER_ACCEPTANCE_EXPECTED_VERSION="$target_version" \
  --env DAKIA_UPDATER_ACCEPTANCE_EXPECT_REJECTION="$expect_rejection" \
  "$app"
initial_pid=""
for _ in $(seq 1 30); do
  initial_pid="$(pgrep -f "$executable" | tail -1 || true)"
  [[ -n "$initial_pid" ]] && break
  sleep 1
done
[[ -n "$initial_pid" ]]

for _ in $(seq 1 360); do
  if [[ -s "$evidence" ]] &&
    jq -e --arg event "$expected_terminal_event" \
      'select(.event == $event)' "$evidence" >/dev/null; then
    break
  fi
  if [[ -s "$evidence" ]] &&
    jq -e 'select(.event == "failed")' "$evidence" >/dev/null; then
    cat "$evidence" >&2
    cat "$output" >&2
    exit 1
  fi
  sleep 1
done

jq -e --arg event "$expected_terminal_event" \
  'select(.event == $event)' "$evidence" >/dev/null
if [[ "$mode" == "valid" ]]; then
  jq -s -e --arg version "$target_version" '
    map(.event) as $events |
    ($events | index("update-available")) <
      ($events | index("downloaded")) and
    ($events | index("downloaded")) <
      ($events | index("installing")) and
    ($events | index("installing")) <
      ($events | rindex("launched")) and
    (last.event == "completed") and
    (last.detail == $version)
  ' "$evidence" >/dev/null
  final_version="$(/usr/libexec/PlistBuddy -c \
    'Print :CFBundleShortVersionString' "$app/Contents/Info.plist")"
  [[ "$final_version" == "$target_version" ]]
  current_pid="$(pgrep -f "$executable" | tail -1)"
  [[ -n "$current_pid" && "$current_pid" != "$initial_pid" ]]
else
  final_version="$(/usr/libexec/PlistBuddy -c \
    'Print :CFBundleShortVersionString' "$app/Contents/Info.plist")"
  [[ "$final_version" == "$baseline_version" ]]
fi

pkill -f "$executable" >/dev/null 2>&1 || true
"$root_dir/scripts/snapshot-updater-acceptance-profile.sh" "$data_dir" |
  tee "$profile_after"
cmp "$profile_before" "$profile_after"
codesign --verify --deep --strict --verbose=2 "$app"
spctl --assess --type execute --verbose=2 "$app"
xcrun stapler validate "$app"

cp "$evidence" "$result_dir/evidence.jsonl"
cp "$output" "$result_dir/app.log"
cp "$profile_before" "$result_dir/profile-before.txt"
cp "$profile_after" "$result_dir/profile-after.txt"
evidence_sha="$(shasum -a 256 "$result_dir/evidence.jsonl" | awk '{print $1}')"
profile_sha="$(shasum -a 256 "$result_dir/profile-before.txt" | awk '{print $1}')"
jq -n \
  --arg schema "dakia-local-updater-acceptance-v1" \
  --arg arch "$arch" \
  --arg mode "$mode" \
  --arg baseline_tag "$baseline_tag" \
  --arg target_tag "$target_tag" \
  --arg final_version "$final_version" \
  --arg completed_at "$(date -u +'%Y-%m-%dT%H:%M:%SZ')" \
  --arg evidence_sha256 "$evidence_sha" \
  --arg profile_sha256 "$profile_sha" \
  --arg machine "$(scutil --get ComputerName 2>/dev/null || hostname)" \
  '{
    schema: $schema,
    result: "passed",
    arch: $arch,
    mode: $mode,
    baseline_tag: $baseline_tag,
    target_tag: $target_tag,
    final_version: $final_version,
    completed_at: $completed_at,
    evidence_sha256: $evidence_sha256,
    profile_sha256: $profile_sha256,
    machine: $machine
  }' >"$result_dir/result.json"

echo "Native $mode updater acceptance passed: $result_dir/result.json"
