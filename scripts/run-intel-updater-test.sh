#!/usr/bin/env bash
set -euo pipefail

target_tag="${1:-}"
mode="${2:-}"
kit_root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
baseline_tag="$(<"$kit_root/baseline-tag.txt")"
baseline_version="${baseline_tag#v}"

if [[ "$(uname -m)" != "x86_64" ]]; then
  echo "This kit must run natively on the Intel MacBook." >&2
  exit 1
fi
if [[ -z "$target_tag" || -z "$mode" ]]; then
  echo "Usage: $0 vX.Y.Z <valid|tampered-archive|invalid-signature>" >&2
  exit 2
fi

DAKIA_SKIP_FIXTURE_PUBLISH=1 \
  "$kit_root/scripts/run-local-updater-acceptance.sh" \
    "$baseline_tag" "$target_tag" "$mode" \
    "$kit_root/Dakia_${baseline_version}_x64.dmg" \
    - "$kit_root/evidence"

echo "Copy $kit_root/evidence back to the release Mac after all three modes pass."
