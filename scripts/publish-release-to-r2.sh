#!/usr/bin/env bash
set -euo pipefail

tag="${1:-}"
source_dir="${2:-}"
bucket="dakia-releases"
download_origin="https://downloads.dakiamail.com"
root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ ! "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ || \
      -z "$source_dir" ]]; then
  echo "Usage: $0 vX.Y.Z /path/to/local-release-assets" >&2
  exit 1
fi
if ! command -v aws >/dev/null 2>&1; then
  echo "AWS CLI is required for the bucket-scoped R2 S3 credentials." >&2
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

apple_dmg="$asset_dir/Dakia_${version}_aarch64.dmg"
apple_update="$asset_dir/Dakia-aarch64.app.tar.gz"
apple_signature="$apple_update.sig"

for artifact in \
  "$apple_dmg" "$apple_update" "$apple_signature"; do
  if [[ ! -s "$artifact" ]]; then
    echo "Missing or empty release artifact: $artifact" >&2
    exit 1
  fi
done

"$root_dir/scripts/verify-macos-release-dmg.sh" "$apple_dmg"

release_prefix="macos/$tag"
manifest_key="macos/latest/latest.json"
apple_dmg_key="$release_prefix/Dakia-Apple-Silicon.dmg"
apple_update_key="$release_prefix/Dakia-aarch64.app.tar.gz"
apple_signature_key="$apple_update_key.sig"

immutable_keys=(
  "$apple_dmg_key"
  "$apple_update_key"
  "$apple_signature_key"
)
for key in "${immutable_keys[@]}"; do
  status="$(curl --silent --show-error --head --output /dev/null \
    --write-out '%{http_code}' \
    "$download_origin/$key?immutable-preflight=$tag")"
  if [[ "$status" != "404" ]]; then
    echo "Refusing to overwrite immutable object $key (HTTP $status)." >&2
    exit 1
  fi
done

upload() {
  local key="$1"
  local file="$2"
  local content_type="$3"
  local filename="$4"
  local cache_control="$5"

  AWS_ACCESS_KEY_ID="$R2_ACCESS_KEY_ID" \
    AWS_SECRET_ACCESS_KEY="$R2_SECRET_ACCESS_KEY" \
    AWS_DEFAULT_REGION="auto" \
    aws s3 cp "$file" "s3://$bucket/$key" \
      --endpoint-url "$r2_endpoint" \
      --content-type "$content_type" \
      --content-disposition "attachment; filename=\"$filename\"" \
      --cache-control "$cache_control" \
      --no-progress
}

verify_public_copy() {
  local key="$1"
  local source="$2"
  local downloaded="$work_dir/$(basename "$key").downloaded"
  local status
  status="$(curl --silent --show-error --location --output "$downloaded" \
    --write-out '%{http_code}' "$download_origin/$key?release-gate=$tag")"
  if [[ "$status" != "200" ]]; then
    echo "Public release gate failed for $key (HTTP $status)." >&2
    exit 1
  fi
  if ! cmp -s "$source" "$downloaded"; then
    echo "Public artifact bytes do not match the signed source: $key" >&2
    exit 1
  fi
}

immutable_cache="public, max-age=31536000, immutable"
upload "$apple_dmg_key" "$apple_dmg" "application/x-apple-diskimage" \
  "Dakia-$version-Apple-Silicon.dmg" "$immutable_cache"
upload "$apple_update_key" "$apple_update" "application/gzip" \
  "Dakia-$version-aarch64.app.tar.gz" "$immutable_cache"
upload "$apple_signature_key" "$apple_signature" "text/plain; charset=utf-8" \
  "Dakia-$version-aarch64.app.tar.gz.sig" "$immutable_cache"

# Anonymous downloads must return the exact bytes that were signed before the
# stable updater feed is allowed to move.
verify_public_copy "$apple_update_key" "$apple_update"
verify_public_copy "$apple_signature_key" "$apple_signature"
verify_public_copy "$apple_dmg_key" "$apple_dmg"

notes_file="$asset_dir/release-notes.md"
if [[ ! -s "$notes_file" ]]; then
  notes_file="$work_dir/release-notes.md"
  printf 'Dakia %s\n' "$tag" >"$notes_file"
fi
manifest="$work_dir/latest.json"
node "$root_dir/scripts/updater-manifest.mjs" create \
  --version "$version" \
  --pub-date "$(date -u +'%Y-%m-%dT%H:%M:%SZ')" \
  --notes-file "$notes_file" \
  --aarch64-url "$download_origin/$apple_update_key" \
  --aarch64-signature-file "$apple_signature" \
  --tauri-config "$root_dir/apps/desktop/src-tauri/tauri.conf.json" \
  --output "$manifest"
node "$root_dir/scripts/updater-manifest.mjs" validate \
  --manifest "$manifest" \
  --tauri-config "$root_dir/apps/desktop/src-tauri/tauri.conf.json"

# The manifest is the final write, so clients remain on the preceding release
# if any artifact publication or verification fails.
upload "macos/latest/Dakia-Apple-Silicon.dmg" "$apple_dmg" \
  "application/x-apple-diskimage" "Dakia-$version-Apple-Silicon.dmg" \
  "no-store, max-age=0"
upload "$manifest_key" "$manifest" \
  "application/json; charset=utf-8" "latest.json" "no-store, max-age=0"

manifest_status="$(curl --silent --show-error --location --output "$work_dir/public-latest.json" \
  --write-out '%{http_code}' \
  "$download_origin/$manifest_key?release-gate=$tag")"
if [[ "$manifest_status" != "200" ]]; then
  echo "Published updater manifest is not anonymously reachable (HTTP $manifest_status)." >&2
  exit 1
fi
node "$root_dir/scripts/updater-manifest.mjs" validate \
  --manifest "$work_dir/public-latest.json" \
  --tauri-config "$root_dir/apps/desktop/src-tauri/tauri.conf.json"
if ! cmp -s "$manifest" "$work_dir/public-latest.json"; then
  echo "Public updater manifest does not match the generated manifest." >&2
  exit 1
fi

echo "Published signed Apple Silicon updater and download for $tag."
echo "Updater manifest: $download_origin/$manifest_key"
