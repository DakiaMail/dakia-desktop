#!/usr/bin/env bash
set -euo pipefail

tag="${1:-}"
asset_dir="${2:-}"
evidence_root="${3:-}"
root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ -z "$tag" || -z "$asset_dir" || -z "$evidence_root" ]]; then
  echo "Usage: $0 vX.Y.Z /path/to/production-assets /path/to/evidence-root" >&2
  exit 2
fi

"$root_dir/scripts/verify-local-updater-evidence.sh" \
  "$tag" "$evidence_root"
# shellcheck source=local-release-env.sh
source "$root_dir/scripts/local-release-env.sh"
dakia_require_r2_environment
DAKIA_UPDATER_CHANNEL=production \
DAKIA_PRODUCTION_EVIDENCE_VERIFIED=1 \
  "$root_dir/scripts/publish-release-to-r2.sh" "$tag" "$asset_dir"
