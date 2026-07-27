#!/usr/bin/env bash
set -euo pipefail

target_tag="${1:-}"
arch="${2:-}"
mode="${3:-}"
evidence_root="${4:-}"
reason="${5:-}"
authorized_by="${6:-}"

if [[ ! "$target_tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ || \
      ( "$arch" != "aarch64" && "$arch" != "x86_64" ) || \
      ( "$mode" != "valid" && "$mode" != "tampered-archive" && \
        "$mode" != "invalid-signature" ) || \
      -z "$evidence_root" || -z "$reason" || -z "$authorized_by" ]]; then
  echo "Usage: $0 vX.Y.Z <aarch64|x86_64> <valid|tampered-archive|invalid-signature> <evidence-root> <reason> <authorized-by>" >&2
  exit 2
fi
if [[ "$mode" == "valid" && "${DAKIA_ALLOW_VALID_WAIVER:-0}" != "1" ]]; then
  echo "Waiving install-and-restart evidence requires DAKIA_ALLOW_VALID_WAIVER=1." >&2
  exit 1
fi

result_dir="$evidence_root/$arch/$mode"
waiver="$result_dir/waiver.json"
if [[ -e "$result_dir/result.json" || -e "$waiver" ]]; then
  echo "Refusing to replace existing acceptance result or waiver: $result_dir" >&2
  exit 1
fi

mkdir -p "$result_dir"
temporary="$waiver.tmp.$$"
trap 'rm -f "$temporary"' EXIT HUP INT TERM
jq -n \
  --arg schema "dakia-local-updater-waiver-v1" \
  --arg result "waived" \
  --arg arch "$arch" \
  --arg mode "$mode" \
  --arg target_tag "$target_tag" \
  --arg reason "$reason" \
  --arg authorized_by "$authorized_by" \
  --arg authorized_at "$(date -u +'%Y-%m-%dT%H:%M:%SZ')" \
  --argjson risk_acknowledged "$([[ "$mode" == "valid" ]] && echo true || echo false)" \
  '{
    schema: $schema,
    result: $result,
    arch: $arch,
    mode: $mode,
    target_tag: $target_tag,
    reason: $reason,
    authorized_by: $authorized_by,
    authorized_at: $authorized_at,
    risk_acknowledged: $risk_acknowledged
  }' >"$temporary"
mv "$temporary" "$waiver"
trap - EXIT HUP INT TERM

echo "Recorded updater acceptance waiver: $waiver"
