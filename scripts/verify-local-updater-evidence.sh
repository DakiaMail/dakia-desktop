#!/usr/bin/env bash
set -euo pipefail

target_tag="${1:-}"
evidence_root="${2:-}"
if [[ ! "$target_tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ || -z "$evidence_root" ]]; then
  echo "Usage: $0 vX.Y.Z /path/to/evidence-root" >&2
  exit 2
fi
target_version="${target_tag#v}"
passed_count=0
waived_count=0

for arch in aarch64 x86_64; do
  for mode in valid tampered-archive invalid-signature; do
    result_dir="$evidence_root/$arch/$mode"
    result="$result_dir/result.json"
    waiver="$result_dir/waiver.json"
    events="$result_dir/evidence.jsonl"
    before="$result_dir/profile-before.txt"
    after="$result_dir/profile-after.txt"

    if [[ -s "$waiver" ]]; then
      [[ ! -e "$result" ]] || {
        echo "Acceptance result and waiver both exist: $result_dir" >&2
        exit 1
      }
      jq -e \
        --arg arch "$arch" --arg mode "$mode" --arg tag "$target_tag" '
        .schema == "dakia-local-updater-waiver-v1" and
        .result == "waived" and
        .arch == $arch and .mode == $mode and .target_tag == $tag and
        (.reason | type == "string" and length > 0) and
        (.authorized_by | type == "string" and length > 0) and
        (.authorized_at | type == "string" and length > 0) and
        ($mode != "valid" or .risk_acknowledged == true)
      ' "$waiver" >/dev/null
      waived_count=$((waived_count + 1))
      continue
    fi

    for file in "$result" "$events" "$before" "$after"; do
      [[ -s "$file" ]] || {
        echo "Missing local updater evidence: $file" >&2
        exit 1
      }
    done

    jq -e \
      --arg arch "$arch" --arg mode "$mode" --arg tag "$target_tag" '
      .schema == "dakia-local-updater-acceptance-v1" and
      .result == "passed" and
      .arch == $arch and .mode == $mode and .target_tag == $tag
    ' "$result" >/dev/null
    cmp "$before" "$after"
    expected_evidence_sha="$(jq -r .evidence_sha256 "$result")"
    expected_profile_sha="$(jq -r .profile_sha256 "$result")"
    [[ "$(shasum -a 256 "$events" | awk '{print $1}')" == "$expected_evidence_sha" ]]
    [[ "$(shasum -a 256 "$before" | awk '{print $1}')" == "$expected_profile_sha" ]]

    if [[ "$mode" == "valid" ]]; then
      [[ "$(jq -r .final_version "$result")" == "$target_version" ]]
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
      ' "$events" >/dev/null
    else
      baseline_version="$(jq -r '.baseline_tag | ltrimstr("v")' "$result")"
      [[ "$(jq -r .final_version "$result")" == "$baseline_version" ]]
      jq -e 'select(.event == "signature-rejected")' "$events" >/dev/null
    fi
    passed_count=$((passed_count + 1))
  done
done

echo "Verified local updater acceptance for $target_tag: $passed_count passed, $waived_count waived."
