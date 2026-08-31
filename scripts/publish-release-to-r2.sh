#!/usr/bin/env bash
set -euo pipefail

tag="${1:-}"
source_dir="${2:-}"
bucket="dakia-releases"
download_origin="https://downloads.dakiamail.com"
root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# shellcheck source=local-release-env.sh
source "$root_dir/scripts/local-release-env.sh"

if [[ ! "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ || -z "$source_dir" ]]; then
  echo "Usage: $0 vX.Y.Z /path/to/local-release-assets" >&2
  exit 1
fi
if ! command -v aws >/dev/null 2>&1; then
  echo "AWS CLI is required for the bucket-scoped R2 S3 credentials." >&2
  exit 1
fi
if ! command -v curl >/dev/null 2>&1; then
  echo "curl is required for anonymous public-artifact verification." >&2
  exit 1
fi
if [[ -z "${R2_ACCESS_KEY_ID:-}" || -z "${R2_SECRET_ACCESS_KEY:-}" || -z "${CLOUDFLARE_ACCOUNT_ID:-}" ]]; then
  echo "R2_ACCESS_KEY_ID, R2_SECRET_ACCESS_KEY, and CLOUDFLARE_ACCOUNT_ID are required." >&2
  exit 1
fi

version="${tag#v}"
work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT
asset_dir="$(cd "$source_dir" && pwd)"
apple_dmg="$asset_dir/Dakia_${version}_aarch64.dmg"
apple_update="$asset_dir/Dakia-aarch64.app.tar.gz"
apple_signature="$apple_update.sig"
notes_file="$asset_dir/release-notes.md"
checksums_file="$asset_dir/SHA256SUMS.txt"
source_marker="$asset_dir/source-commit.txt"
release_notes_source="$root_dir/docs/releases/$tag.md"

for artifact in "$apple_dmg" "$apple_update" "$apple_signature" "$notes_file" "$checksums_file" "$source_marker"; do
  if [[ ! -s "$artifact" ]]; then
    echo "Missing or empty release artifact: $artifact" >&2
    exit 1
  fi
done

if ! git -C "$root_dir" ls-files --error-unmatch -- "docs/releases/$tag.md" >/dev/null 2>&1 || [[ ! -s "$release_notes_source" ]]; then
  echo "Missing tracked release notes: $release_notes_source" >&2
  exit 1
fi
if ! cmp -s "$release_notes_source" "$notes_file"; then
  echo "Release notes asset does not exactly match tracked notes: $release_notes_source" >&2
  exit 1
fi

# The builder writes this exact, ordered manifest from the final three signed
# artifacts. Comparing it byte-for-byte rejects omitted, substituted, or extra
# checksum records instead of trusting a supplied filename list.
expected_checksums="$work_dir/SHA256SUMS.expected"
(
  cd "$asset_dir"
  shasum -a 256 "Dakia_${version}_aarch64.dmg" "Dakia-aarch64.app.tar.gz" "Dakia-aarch64.app.tar.gz.sig"
) >"$expected_checksums"
if ! cmp -s "$expected_checksums" "$checksums_file"; then
  echo "SHA256SUMS.txt must exactly verify the final DMG, updater archive, and signature." >&2
  exit 1
fi

"$root_dir/scripts/verify-macos-release-dmg.sh" "$apple_dmg"
node "$root_dir/scripts/verify-updater-signature.mjs" "$apple_update" "$apple_signature" "$root_dir/apps/desktop/src-tauri/tauri.conf.json"

updater_verify_dir="$work_dir/updater-archive"
mkdir -p "$updater_verify_dir"
tar -xzf "$apple_update" -C "$updater_verify_dir"
updater_app="$updater_verify_dir/Dakia.app"
if [[ ! -d "$updater_app" ]]; then
  echo "Updater archive is missing Dakia.app." >&2
  exit 1
fi
"$root_dir/scripts/verify-macos-release-app.sh" "$updater_app"
updater_version="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$updater_app/Contents/Info.plist")"
if [[ "$updater_version" != "$version" ]]; then
  echo "Updater app version '$updater_version' does not match '$version'." >&2
  exit 1
fi

verify_source_provenance() {
  local head tag_commit local_tag_object remote_refs remote_tag_object remote_tag_commit branch
  if [[ -n "$(git -C "$root_dir" status --porcelain)" ]]; then
    echo "Release source has uncommitted or untracked changes." >&2
    exit 1
  fi
  branch="$(git -C "$root_dir" branch --show-current)"
  if [[ "$branch" != "main" ]]; then
    echo "Release publication must run from main, not '$branch'." >&2
    exit 1
  fi
  head="$(git -C "$root_dir" rev-parse HEAD)"
  if ! dakia_require_expected_release_origin "$root_dir"; then
    exit 1
  fi
  if [[ "$(tr -d '\n' <"$source_marker")" != "$head" ]]; then
    echo "source-commit.txt does not bind these artifacts to the exact main commit." >&2
    exit 1
  fi
  if ! dakia_require_live_main_provenance "$root_dir"; then
    exit 1
  fi
  tag_commit="$(git -C "$root_dir" rev-parse --verify "refs/tags/$tag^{commit}")"
  if [[ "$tag_commit" != "$head" ]]; then
    echo "Signed source tag $tag must point exactly at HEAD on main." >&2
    exit 1
  fi
  if ! git -C "$root_dir" verify-tag "$tag" >/dev/null; then
    echo "Source tag $tag is not cryptographically verifiable." >&2
    exit 1
  fi
  local_tag_object="$(git -C "$root_dir" rev-parse "refs/tags/$tag")"
  remote_refs="$(git -C "$root_dir" ls-remote --tags origin "refs/tags/$tag" "refs/tags/$tag^{}")"
  remote_tag_object="$(awk -v tag="$tag" '$2 == "refs/tags/" tag { print $1 }' <<<"$remote_refs")"
  remote_tag_commit="$(awk -v tag="$tag" '$2 == "refs/tags/" tag "^{}" { print $1 }' <<<"$remote_refs")"
  if [[ "$remote_tag_object" != "$local_tag_object" ]]; then
    echo "Remote source tag object $tag must match the verified local signed tag." >&2
    exit 1
  fi
  if [[ "$remote_tag_commit" != "$head" ]]; then
    echo "Remote source tag $tag must point exactly at origin/main." >&2
    exit 1
  fi
}

verify_source_provenance

r2_endpoint="https://${CLOUDFLARE_ACCOUNT_ID}.r2.cloudflarestorage.com"
release_prefix="macos/$tag"
manifest_key="macos/latest/latest.json"
stable_dmg_key="macos/latest/Dakia-Apple-Silicon.dmg"
publication_state_key="macos/latest/publication.json"
apple_dmg_key="$release_prefix/Dakia-Apple-Silicon.dmg"
apple_update_key="$release_prefix/Dakia-aarch64.app.tar.gz"
apple_signature_key="$apple_update_key.sig"
curl_options=(--silent --show-error --location --proto '=https' --connect-timeout 15 --max-time 90 --retry 3 --retry-delay 1 --retry-max-time 180)

fetch_public_file() {
  local key="$1" destination="$2" status
  if ! status="$(curl "${curl_options[@]}" --output "$destination" --write-out '%{http_code}' "$download_origin/$key?release-gate=$tag")"; then
    echo "Anonymous download failed for $key." >&2
    return 1
  fi
  printf '%s' "$status"
}

public_copy_matches() {
  local key="$1" source="$2" downloaded status
  downloaded="$work_dir/$(basename "$key").downloaded"
  if ! status="$(fetch_public_file "$key" "$downloaded")"; then
    return 1
  fi
  [[ "$status" == "200" ]] && cmp -s "$source" "$downloaded"
}

verify_public_copy() {
  local key="$1" source="$2"
  if ! public_copy_matches "$key" "$source"; then
    echo "Public artifact is missing or differs from the signed source: $key" >&2
    exit 1
  fi
}

get_authenticated_manifest() {
  local destination="$1" etag
  rm -f "$destination"
  if ! etag="$(
    AWS_ACCESS_KEY_ID="$R2_ACCESS_KEY_ID" AWS_SECRET_ACCESS_KEY="$R2_SECRET_ACCESS_KEY" AWS_DEFAULT_REGION="auto" \
      aws s3api get-object --bucket "$bucket" --key "$manifest_key" --endpoint-url "$r2_endpoint" \
        --no-cli-pager --query ETag --output text "$destination"
  )"; then
    return 1
  fi
  [[ -n "$etag" && -s "$destination" ]] || return 1
  printf '%s' "$etag"
}

public_manifest_converges_to() {
  local authoritative="$1" public_copy="$2" status attempt
  for attempt in 1 2 3 4 5; do
    if status="$(fetch_public_file "$manifest_key" "$public_copy")" && \
      [[ "$status" == "200" ]] && cmp -s "$authoritative" "$public_copy"; then
      return 0
    fi
    if [[ "$attempt" != "5" ]]; then
      sleep 1
    fi
  done
  return 1
}

version_relation() {
  node - "$1" "$2" <<'NODE'
const [candidate, current] = process.argv.slice(2);
const parse = (value) => {
  const match = value.match(/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/);
  if (!match || match[4]?.split(".").some((part) => /^0\d+$/.test(part))) throw new Error(`invalid SemVer ${value}`);
  return { core: match.slice(1, 4).map(Number), pre: match[4]?.split(".") ?? [] };
};
const compareIdentifier = (left, right) => {
  const leftNumeric = /^\d+$/.test(left); const rightNumeric = /^\d+$/.test(right);
  if (leftNumeric && rightNumeric) return Number(left) - Number(right);
  if (leftNumeric) return -1; if (rightNumeric) return 1;
  return left < right ? -1 : left > right ? 1 : 0;
};
const compare = (left, right) => {
  for (let index = 0; index < 3; index += 1) if (left.core[index] !== right.core[index]) return left.core[index] - right.core[index];
  if (!left.pre.length && !right.pre.length) return 0;
  if (!left.pre.length) return 1; if (!right.pre.length) return -1;
  for (let index = 0; index < Math.max(left.pre.length, right.pre.length); index += 1) {
    if (left.pre[index] === undefined) return -1; if (right.pre[index] === undefined) return 1;
    const result = compareIdentifier(left.pre[index], right.pre[index]); if (result !== 0) return result;
  }
  return 0;
};
process.stdout.write(`${Math.sign(compare(parse(candidate), parse(current)))}\n`);
NODE
}

verify_current_manifest_is_candidate() {
  local public_manifest="$1"
  node - "$public_manifest" "$version" "$download_origin/$apple_update_key" "$apple_signature" "$notes_file" <<'NODE'
const { readFileSync } = require("node:fs");
const [manifestPath, version, url, signaturePath, notesPath] = process.argv.slice(2);
const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
const entry = manifest.platforms?.["darwin-aarch64"];
if (manifest.version !== version || entry?.url !== url || entry?.signature?.trim() !== readFileSync(signaturePath, "utf8").trim() || manifest.notes !== readFileSync(notesPath, "utf8").trim()) {
  throw new Error("The existing latest.json is not an exact resume of this candidate.");
}
NODE
}

public_manifest="$work_dir/public-latest-before.json"
manifest_status="$(fetch_public_file "$manifest_key" "$public_manifest")"
if [[ "$manifest_status" != "200" ]]; then
  echo "Public updater manifest must be anonymously reachable before publication (HTTP $manifest_status)." >&2
  exit 1
fi
node "$root_dir/scripts/updater-manifest.mjs" validate --manifest "$public_manifest" --tauri-config "$root_dir/apps/desktop/src-tauri/tauri.conf.json"
authenticated_manifest="$work_dir/authenticated-latest-before.json"
if ! manifest_etag="$(get_authenticated_manifest "$authenticated_manifest")" || \
  ! public_manifest_converges_to "$authenticated_manifest" "$public_manifest"; then
  echo "Authenticated and public updater manifests must be the same current bytes." >&2
  exit 1
fi
existing_version="$(node -e 'process.stdout.write(JSON.parse(require("node:fs").readFileSync(process.argv[1], "utf8")).version)' "$public_manifest")"
relation="$(version_relation "$version" "$existing_version")"
resuming=false
case "$relation" in
  1) ;;
  0) verify_current_manifest_is_candidate "$public_manifest"; resuming=true ;;
  -1) echo "Refusing to publish $tag: public updater version $existing_version is newer." >&2; exit 1 ;;
  *) echo "Unable to compare candidate $version with public updater version $existing_version." >&2; exit 1 ;;
esac

# Enforce the cross-service ordering even when the R2 command is invoked
# directly. Eligibility is checked first so a stale candidate cannot create a
# GitHub draft before being rejected.
"$root_dir/scripts/prepare-github-release-draft.sh" "$tag" "$asset_dir"

upload() {
  local key="$1" file="$2" content_type="$3" filename="$4" cache_control="$5"
  # This call is reached only after the caller's authenticated/public reads.
  # Keep the live-main check adjacent to the mutating copy.
  dakia_require_release_mutation_provenance "$root_dir" || exit 1
  AWS_ACCESS_KEY_ID="$R2_ACCESS_KEY_ID" AWS_SECRET_ACCESS_KEY="$R2_SECRET_ACCESS_KEY" AWS_DEFAULT_REGION="auto" \
    aws s3 cp "$file" "s3://$bucket/$key" --endpoint-url "$r2_endpoint" --content-type "$content_type" \
      --content-disposition "attachment; filename=\"$filename\"" --cache-control "$cache_control" --no-progress
}

immutable_object_matches() {
  local key="$1" source="$2" existing="$work_dir/$(basename "$key").authenticated"
  AWS_ACCESS_KEY_ID="$R2_ACCESS_KEY_ID" AWS_SECRET_ACCESS_KEY="$R2_SECRET_ACCESS_KEY" AWS_DEFAULT_REGION="auto" \
    aws s3api get-object --bucket "$bucket" --key "$key" --endpoint-url "$r2_endpoint" --no-cli-pager "$existing" >/dev/null 2>&1 && cmp -s "$source" "$existing"
}

upload_immutable() {
  local key="$1" file="$2" content_type="$3" filename="$4" cache_control="$5"
  if immutable_object_matches "$key" "$file"; then return; fi
  # Recheck after the authenticated immutable-object read and immediately
  # before the conditional create.
  dakia_require_release_mutation_provenance "$root_dir" || exit 1
  if AWS_ACCESS_KEY_ID="$R2_ACCESS_KEY_ID" AWS_SECRET_ACCESS_KEY="$R2_SECRET_ACCESS_KEY" AWS_DEFAULT_REGION="auto" \
    aws s3api put-object --bucket "$bucket" --key "$key" --body "$file" --content-type "$content_type" \
      --content-disposition "attachment; filename=\"$filename\"" --cache-control "$cache_control" --if-none-match "*" \
      --endpoint-url "$r2_endpoint" --no-cli-pager >/dev/null; then return; fi
  # A concurrent publisher may have created the key after our first read.
  if immutable_object_matches "$key" "$file"; then return; fi
  echo "Refusing to replace immutable object with different or unreadable bytes: $key" >&2
  exit 1
}

publication_state="$work_dir/publication.json"
source_commit="$(git -C "$root_dir" rev-parse HEAD)"
node - "$publication_state" "$tag" "$version" "$source_commit" <<'NODE'
const { writeFileSync } = require("node:fs");
const [output, tag, version, source] = process.argv.slice(2);
writeFileSync(output, `${JSON.stringify({ tag, version, source }, null, 2)}\n`);
NODE

publication_state_is_candidate() {
  local state_file="$1"
  node - "$state_file" "$tag" "$version" "$source_commit" <<'NODE'
const { readFileSync } = require("node:fs");
const [path, tag, version, source] = process.argv.slice(2);
const state = JSON.parse(readFileSync(path, "utf8"));
process.exit(state.tag === tag && state.version === version && state.source === source ? 0 : 1);
NODE
}

get_publication_state() {
  local destination="$1"
  rm -f "$destination"
  AWS_ACCESS_KEY_ID="$R2_ACCESS_KEY_ID" AWS_SECRET_ACCESS_KEY="$R2_SECRET_ACCESS_KEY" AWS_DEFAULT_REGION="auto" \
    aws s3api get-object --bucket "$bucket" --key "$publication_state_key" --endpoint-url "$r2_endpoint" \
      --no-cli-pager --query ETag --output text "$destination" 2>/dev/null
}

claim_publication_state() {
  local existing_state="$work_dir/publication-existing.json" state_etag state_version claimed_state="$work_dir/publication-claimed.json"
  if state_etag="$(get_publication_state "$existing_state")"; then
    if publication_state_is_candidate "$existing_state"; then
      return
    fi
    state_version="$(jq -r '.version // empty' "$existing_state")"
    if [[ "$state_version" != "$existing_version" ]]; then
      echo "Another incomplete release owns the mutable publication state (version ${state_version:-unknown})." >&2
      exit 1
    fi
    dakia_require_release_mutation_provenance "$root_dir" || exit 1
    if AWS_ACCESS_KEY_ID="$R2_ACCESS_KEY_ID" AWS_SECRET_ACCESS_KEY="$R2_SECRET_ACCESS_KEY" AWS_DEFAULT_REGION="auto" \
      aws s3api put-object --bucket "$bucket" --key "$publication_state_key" --body "$publication_state" \
        --content-type "application/json; charset=utf-8" --cache-control "no-store, max-age=0" \
        --if-match "$state_etag" --endpoint-url "$r2_endpoint" --no-cli-pager >/dev/null; then
      return
    fi
  else
    dakia_require_release_mutation_provenance "$root_dir" || exit 1
    if AWS_ACCESS_KEY_ID="$R2_ACCESS_KEY_ID" AWS_SECRET_ACCESS_KEY="$R2_SECRET_ACCESS_KEY" AWS_DEFAULT_REGION="auto" \
      aws s3api put-object --bucket "$bucket" --key "$publication_state_key" --body "$publication_state" \
        --content-type "application/json; charset=utf-8" --cache-control "no-store, max-age=0" \
        --if-none-match "*" --endpoint-url "$r2_endpoint" --no-cli-pager >/dev/null; then
      return
    fi
  fi

  # A concurrent copy of this exact candidate may have won the claim. Anything
  # else is a hard stop before either mutable release object changes.
  get_publication_state "$claimed_state" >/dev/null || {
    echo "Could not verify the current mutable publication owner." >&2
    exit 1
  }
  publication_state_is_candidate "$claimed_state" || {
    echo "A different release won the mutable publication claim." >&2
    exit 1
  }
}

immutable_cache="public, max-age=31536000, immutable"
upload_immutable "$apple_dmg_key" "$apple_dmg" "application/x-apple-diskimage" "Dakia-$version-Apple-Silicon.dmg" "$immutable_cache"
upload_immutable "$apple_update_key" "$apple_update" "application/gzip" "Dakia-$version-aarch64.app.tar.gz" "$immutable_cache"
upload_immutable "$apple_signature_key" "$apple_signature" "text/plain; charset=utf-8" "Dakia-$version-aarch64.app.tar.gz.sig" "$immutable_cache"

# Anonymous downloads must match the signed source before either mutable object can move.
verify_public_copy "$apple_update_key" "$apple_update"
verify_public_copy "$apple_signature_key" "$apple_signature"
verify_public_copy "$apple_dmg_key" "$apple_dmg"

# Serialize the stable DMG + latest.json pair. If a run stops after claiming,
# only the same tag/source may resume until latest.json reaches that version.
claim_publication_state

winner_dmg_key() {
  local manifest_file="$1"
  node - "$manifest_file" "$download_origin" <<'NODE'
const { readFileSync } = require("node:fs");
const [manifestPath, origin] = process.argv.slice(2);
const url = new URL(JSON.parse(readFileSync(manifestPath, "utf8")).platforms["darwin-aarch64"].url);
const expectedOrigin = new URL(origin).origin;
const match = url.origin === expectedOrigin && url.pathname.match(/^\/macos\/(v[0-9]+\.[0-9]+\.[0-9]+(?:[+-][0-9A-Za-z.-]+)?)\/Dakia-aarch64\.app\.tar\.gz$/);
if (!match) throw new Error("Winning updater manifest does not reference an expected immutable R2 updater.");
process.stdout.write(`macos/${match[1]}/Dakia-Apple-Silicon.dmg`);
NODE
}

repair_stable_alias_to_manifest() {
  local winner_manifest="$1"
  local winner_key winner_dmg="$work_dir/winner-Dakia-Apple-Silicon.dmg.authenticated"
  local after_repair_authenticated="$work_dir/authenticated-latest-after-stable-repair.json"
  local after_repair_public="$work_dir/public-latest-after-stable-repair.json"
  winner_key="$(winner_dmg_key "$winner_manifest")"
  AWS_ACCESS_KEY_ID="$R2_ACCESS_KEY_ID" AWS_SECRET_ACCESS_KEY="$R2_SECRET_ACCESS_KEY" AWS_DEFAULT_REGION="auto" \
    aws s3api get-object --bucket "$bucket" --key "$winner_key" --endpoint-url "$r2_endpoint" \
      --no-cli-pager "$winner_dmg" >/dev/null
  [[ -s "$winner_dmg" ]] || {
    echo "Winning immutable DMG is empty: $winner_key" >&2
    return 1
  }
  verify_public_copy "$winner_key" "$winner_dmg"
  if ! public_copy_matches "$stable_dmg_key" "$winner_dmg"; then
    upload "$stable_dmg_key" "$winner_dmg" "application/x-apple-diskimage" "Dakia-Apple-Silicon.dmg" "no-store, max-age=0"
  fi
  verify_public_copy "$stable_dmg_key" "$winner_dmg"
  if ! get_authenticated_manifest "$after_repair_authenticated" >/dev/null; then
    echo "Could not re-read the authenticated updater manifest after stable DMG repair." >&2
    return 1
  fi
  node "$root_dir/scripts/updater-manifest.mjs" validate --manifest "$after_repair_authenticated" --tauri-config "$root_dir/apps/desktop/src-tauri/tauri.conf.json"
  if ! public_manifest_converges_to "$after_repair_authenticated" "$after_repair_public" || \
    ! cmp -s "$winner_manifest" "$after_repair_authenticated"; then
    echo "Updater manifest changed while repairing the stable DMG alias." >&2
    return 1
  fi
}

# Every artifact gate, including public stable-DMG byte verification, passes
# before latest.json changes. A failed stable upload therefore leaves clients
# on the preceding manifest. A final CAS loser repairs the alias to its winner.
if ! public_copy_matches "$stable_dmg_key" "$apple_dmg"; then
  upload "$stable_dmg_key" "$apple_dmg" "application/x-apple-diskimage" "Dakia-$version-Apple-Silicon.dmg" "no-store, max-age=0"
fi
verify_public_copy "$stable_dmg_key" "$apple_dmg"

if [[ "$resuming" == true ]]; then
  echo "Verified exact R2 resume for $tag; latest.json and stable DMG are current."
  exit 0
fi

manifest="$work_dir/latest.json"
node "$root_dir/scripts/updater-manifest.mjs" create --version "$version" --pub-date "$(date -u +'%Y-%m-%dT%H:%M:%SZ')" \
  --notes-file "$notes_file" --aarch64-url "$download_origin/$apple_update_key" --aarch64-signature-file "$apple_signature" \
  --tauri-config "$root_dir/apps/desktop/src-tauri/tauri.conf.json" --output "$manifest"
node "$root_dir/scripts/updater-manifest.mjs" validate --manifest "$manifest" --tauri-config "$root_dir/apps/desktop/src-tauri/tauri.conf.json"

# This is the final normal-path R2 mutation. Never mutate an R2 object after a
# successful CAS; only a losing CAS can enter the explicit winner-repair path.
manifest_written=false
if ! dakia_require_release_mutation_provenance "$root_dir"; then
  exit 1
fi
if AWS_ACCESS_KEY_ID="$R2_ACCESS_KEY_ID" AWS_SECRET_ACCESS_KEY="$R2_SECRET_ACCESS_KEY" AWS_DEFAULT_REGION="auto" \
  aws s3api put-object --bucket "$bucket" --key "$manifest_key" --body "$manifest" \
    --content-type "application/json; charset=utf-8" \
    --content-disposition 'attachment; filename="latest.json"' \
    --cache-control "no-store, max-age=0" --if-match "$manifest_etag" \
    --endpoint-url "$r2_endpoint" --no-cli-pager >/dev/null; then
  manifest_written=true
fi

authoritative_winner_manifest="$work_dir/authenticated-latest-after.json"
published_manifest="$work_dir/public-latest-after.json"
if ! get_authenticated_manifest "$authoritative_winner_manifest" >/dev/null; then
  echo "Could not read the authenticated updater manifest after the final CAS." >&2
  exit 1
fi
node "$root_dir/scripts/updater-manifest.mjs" validate --manifest "$authoritative_winner_manifest" --tauri-config "$root_dir/apps/desktop/src-tauri/tauri.conf.json"
if [[ "$manifest_written" == true ]]; then
  if ! cmp -s "$manifest" "$authoritative_winner_manifest" || \
    ! public_manifest_converges_to "$authoritative_winner_manifest" "$published_manifest" || \
    ! public_copy_matches "$stable_dmg_key" "$apple_dmg"; then
    echo "Successful latest.json CAS is not publicly paired with the candidate stable DMG." >&2
    exit 1
  fi
elif verify_current_manifest_is_candidate "$authoritative_winner_manifest"; then
  if ! public_manifest_converges_to "$authoritative_winner_manifest" "$published_manifest"; then
    echo "Public updater manifest did not converge to the identical concurrent candidate." >&2
    exit 1
  fi
  verify_public_copy "$stable_dmg_key" "$apple_dmg"
else
  repair_stable_alias_to_manifest "$authoritative_winner_manifest" || {
    echo "Could not prove stable DMG repair for the manifest that won the final CAS." >&2
    exit 1
  }
  echo "Another release won latest.json; repaired its stable DMG alias and stopped." >&2
  exit 1
fi

echo "Published signed Apple Silicon updater and download for $tag."
echo "Updater manifest: $download_origin/$manifest_key"
