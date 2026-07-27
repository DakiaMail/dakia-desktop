#!/usr/bin/env bash
set -euo pipefail

baseline_tag="${1:-}"
baseline_dmg="${2:-}"
output_dir="${3:-}"
root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ ! "$baseline_tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ || \
      ! -s "$baseline_dmg" || -z "$output_dir" ]]; then
  echo "Usage: $0 vX.Y.Z /path/to/intel-baseline.dmg /path/to/new-kit-dir" >&2
  exit 2
fi
if [[ -e "$output_dir" ]]; then
  echo "Refusing to overwrite Intel test kit: $output_dir" >&2
  exit 1
fi

mkdir -p "$output_dir/scripts"
cp "$baseline_dmg" "$output_dir/Dakia_${baseline_tag#v}_x64.dmg"
cp "$root_dir/scripts/run-intel-updater-test.sh" "$output_dir/run-test.sh"
cp "$root_dir/scripts/run-local-updater-acceptance.sh" "$output_dir/scripts/"
cp "$root_dir/scripts/verify-macos-release-dmg.sh" "$output_dir/scripts/"
cp "$root_dir/scripts/verify-macos-release-app.sh" "$output_dir/scripts/"
cp "$root_dir/scripts/seed-updater-acceptance-profile.sh" "$output_dir/scripts/"
cp "$root_dir/scripts/snapshot-updater-acceptance-profile.sh" "$output_dir/scripts/"
cp "$root_dir/scripts/updater-acceptance-profile.py" "$output_dir/scripts/"
printf '%s\n' "$baseline_tag" >"$output_dir/baseline-tag.txt"
chmod +x "$output_dir/run-test.sh" "$output_dir/scripts/"*.sh

archive="${output_dir%/}.tar.gz"
COPYFILE_DISABLE=1 tar -czf "$archive" -C "$(dirname "$output_dir")" \
  "$(basename "$output_dir")"
(
  cd "$(dirname "$archive")"
  shasum -a 256 "$(basename "$archive")" >"$(basename "$archive").sha256"
)
echo "Prepared portable Intel updater test kit: $archive"
