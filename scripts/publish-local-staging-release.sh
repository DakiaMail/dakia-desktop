#!/usr/bin/env bash
set -euo pipefail

tag="${1:-}"
asset_dir="${2:-}"
root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ -z "$tag" || -z "$asset_dir" ]]; then
  echo "Usage: $0 vX.Y.Z /path/to/staging-assets" >&2
  exit 2
fi
# shellcheck source=local-release-env.sh
source "$root_dir/scripts/local-release-env.sh"
dakia_require_r2_environment
DAKIA_UPDATER_CHANNEL=staging \
  "$root_dir/scripts/publish-release-to-r2.sh" "$tag" "$asset_dir"
