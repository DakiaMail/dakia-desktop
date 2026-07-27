#!/usr/bin/env bash
set -euo pipefail

tag="${1:-}"
mode="${2:-}"
source_dir="${3:-}"
bucket="dakia-releases"
download_origin="https://downloads.dakiamail.com"
root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ ! "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ || \
      -z "$source_dir" ]]; then
  echo "Usage: $0 vX.Y.Z <valid|tampered-archive|invalid-signature> /path/to/local-assets" >&2
  exit 2
fi
if [[ "$mode" != "valid" && "$mode" != "tampered-archive" && \
      "$mode" != "invalid-signature" ]]; then
  echo "Fixture mode must be valid, tampered-archive, or invalid-signature." >&2
  exit 2
fi
if ! command -v aws >/dev/null 2>&1; then
  echo "AWS CLI is required." >&2
  exit 1
fi
if [[ -z "${R2_ACCESS_KEY_ID:-}" || -z "${R2_SECRET_ACCESS_KEY:-}" || \
      -z "${CLOUDFLARE_ACCOUNT_ID:-}" ]]; then
  echo "R2_ACCESS_KEY_ID, R2_SECRET_ACCESS_KEY, and CLOUDFLARE_ACCOUNT_ID are required." >&2
  exit 1
fi

version="${tag#v}"
work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT
r2_endpoint="https://${CLOUDFLARE_ACCOUNT_ID}.r2.cloudflarestorage.com"
asset_dir="$(cd "$source_dir" && pwd)"

apple_update="$asset_dir/Dakia-aarch64.app.tar.gz"
intel_update="$asset_dir/Dakia-x86_64.app.tar.gz"
apple_signature="$apple_update.sig"
intel_signature="$intel_update.sig"
for artifact in \
  "$apple_update" "$intel_update" "$apple_signature" "$intel_signature"; do
  if [[ ! -s "$artifact" ]]; then
    echo "Missing or empty updater fixture input: $artifact" >&2
    exit 1
  fi
done

upload() {
  local key="$1"
  local file="$2"
  local content_type="$3"
  local cache_control="$4"

  AWS_ACCESS_KEY_ID="$R2_ACCESS_KEY_ID" \
    AWS_SECRET_ACCESS_KEY="$R2_SECRET_ACCESS_KEY" \
    AWS_DEFAULT_REGION="auto" \
    aws s3 cp "$file" "s3://$bucket/$key" \
      --endpoint-url "$r2_endpoint" \
      --content-type "$content_type" \
      --cache-control "$cache_control" \
      --no-progress
}

verify_public_copy() {
  local key="$1"
  local source="$2"
  local destination="$work_dir/$(basename "$key").downloaded"
  local http_code
  http_code="$(curl --silent --show-error --location --output "$destination" \
    --write-out '%{http_code}' "$download_origin/$key?fixture-gate=$tag-$mode")"
  if [[ "$http_code" != "200" ]]; then
    echo "Updater fixture URL failed for $key (HTTP $http_code)." >&2
    exit 1
  fi
  if ! cmp -s "$source" "$destination"; then
    echo "Updater fixture bytes do not match for $key." >&2
    exit 1
  fi
}

release_prefix="macos/staging/$tag"
apple_url="$download_origin/$release_prefix/Dakia-aarch64.app.tar.gz"
intel_url="$download_origin/$release_prefix/Dakia-x86_64.app.tar.gz"

verify_public_copy "$release_prefix/Dakia-aarch64.app.tar.gz" "$apple_update"
verify_public_copy "$release_prefix/Dakia-x86_64.app.tar.gz" "$intel_update"

if [[ "$mode" == "tampered-archive" ]]; then
  tamper_prefix="macos/staging/tampered/$tag"
  tampered_apple="$work_dir/Dakia-aarch64.app.tar.gz"
  tampered_intel="$work_dir/Dakia-x86_64.app.tar.gz"
  cp "$apple_update" "$tampered_apple"
  cp "$intel_update" "$tampered_intel"
  printf '\0DAKIA_TAMPER_TEST\n' >>"$tampered_apple"
  printf '\0DAKIA_TAMPER_TEST\n' >>"$tampered_intel"

  for fixture in \
    "Dakia-aarch64.app.tar.gz:$tampered_apple" \
    "Dakia-x86_64.app.tar.gz:$tampered_intel"; do
    key="$tamper_prefix/${fixture%%:*}"
    file="${fixture#*:}"
    http_code="$(curl --silent --show-error --head --output /dev/null \
      --write-out '%{http_code}' "$download_origin/$key?fixture-preflight=$tag")"
    if [[ "$http_code" == "404" ]]; then
      upload "$key" "$file" "application/gzip" \
        "public, max-age=31536000, immutable"
    elif [[ "$http_code" != "200" ]]; then
      echo "Unexpected immutable fixture status for $key (HTTP $http_code)." >&2
      exit 1
    fi
    verify_public_copy "$key" "$file"
  done

  apple_url="$download_origin/$tamper_prefix/Dakia-aarch64.app.tar.gz"
  intel_url="$download_origin/$tamper_prefix/Dakia-x86_64.app.tar.gz"
fi

notes_file="$asset_dir/release-notes.md"
if [[ ! -s "$notes_file" ]]; then
  notes_file="$work_dir/release-notes.md"
  printf 'Dakia %s staging updater verification\n' "$tag" >"$notes_file"
fi
valid_manifest="$work_dir/valid-latest.json"
manifest="$work_dir/latest.json"
node "$root_dir/scripts/updater-manifest.mjs" create \
  --version "$version" \
  --pub-date "$(date -u +'%Y-%m-%dT%H:%M:%SZ')" \
  --notes-file "$notes_file" \
  --aarch64-url "$apple_url" \
  --aarch64-signature-file "$apple_signature" \
  --x86-64-url "$intel_url" \
  --x86-64-signature-file "$intel_signature" \
  --output "$valid_manifest"

if [[ "$mode" == "invalid-signature" ]]; then
  node "$root_dir/scripts/updater-manifest.mjs" corrupt-signatures \
    --manifest "$valid_manifest" \
    --output "$manifest"
else
  cp "$valid_manifest" "$manifest"
fi
node "$root_dir/scripts/updater-manifest.mjs" validate --manifest "$manifest"

manifest_key="macos/staging/latest.json"
upload "$manifest_key" "$manifest" "application/json; charset=utf-8" \
  "no-store, max-age=0"
verify_public_copy "$manifest_key" "$manifest"

echo "Published $mode staging updater fixture for $tag."
