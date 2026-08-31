#!/usr/bin/env bash
set -euo pipefail

tag="${1:-}"
source_dir="${2:-}"
root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
release_repo="DakiaMail/dakia-desktop"
download_origin="https://downloads.dakiamail.com"

# shellcheck source=local-release-env.sh
source "$root_dir/scripts/local-release-env.sh"

die() {
  echo "$*" >&2
  exit 1
}

if [[ ! "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ || -z "$source_dir" ]]; then
  die "Usage: $0 vX.Y.Z /path/to/local-release-assets"
fi

for command in git gh jq shasum curl node ssh-keygen; do
  command -v "$command" >/dev/null 2>&1 || die "Required command is unavailable: $command"
done

asset_dir="$(cd "$source_dir" 2>/dev/null && pwd)" ||
  die "Release asset directory does not exist: $source_dir"
version="${tag#v}"
apple_dmg="Dakia_${version}_aarch64.dmg"
apple_update="Dakia-aarch64.app.tar.gz"
apple_signature="Dakia-aarch64.app.tar.gz.sig"
checksums="SHA256SUMS.txt"
notes="release-notes.md"
source_marker="source-commit.txt"
expected_assets="$(printf '%s\n' "$apple_dmg" "$apple_update" "$apple_signature" "$checksums" | LC_ALL=C sort)"
curl_options=(--silent --show-error --location --proto '=https' --connect-timeout 15 --max-time 90 --retry 3 --retry-delay 1 --retry-max-time 180)

require_main_provenance() {
  local status
  status="$(git -C "$root_dir" status --porcelain=v1 --untracked-files=all)"
  [[ -z "$status" ]] || die "Release checkout is not clean (including untracked files)."
  [[ "$(git -C "$root_dir" branch --show-current)" == "main" ]] ||
    die "GitHub Release publishing must run from the local main branch."
  dakia_require_expected_release_origin "$root_dir" || exit 1
  dakia_require_live_main_provenance "$root_dir" || exit 1
}

require_exact_signed_tag() {
  local head tag_commit tag_contents local_tag_object remote_refs remote_tag_object remote_tag_commit signing_format signing_key signing_fingerprint allowed_signers exact_signers
  head="$(git -C "$root_dir" rev-parse HEAD)"
  git -C "$root_dir" rev-parse --verify "$tag^{tag}" >/dev/null 2>&1 ||
    die "Release tag must be an annotated SSH-signed tag: $tag"
  tag_commit="$(git -C "$root_dir" rev-parse "$tag^{commit}")"
  [[ "$tag_commit" == "$head" ]] || die "Release tag $tag does not target HEAD."
  signing_format="$(git -C "$root_dir" config --get gpg.format 2>/dev/null || true)"
  [[ "$signing_format" == "ssh" ]] || die "Git tag signing must use gpg.format=ssh."
  signing_key="$(git -C "$root_dir" config --get user.signingkey 2>/dev/null || true)"
  [[ -n "$signing_key" && -r "$signing_key" ]] || die "A readable Git user.signingkey is required for SSH tag verification."
  signing_fingerprint="$(ssh-keygen -lf "$signing_key" | awk '{ print $2 }')"
  [[ "$signing_fingerprint" == "SHA256:kN9R3QFJZbrE5i2HjEpp+ns5ZNxBTuFySvFx8Ldf/gE" ]] ||
    die "Git tag signing key does not match the trusted Dakia release key."
  tag_contents="$(git -C "$root_dir" cat-file tag "$tag")" ||
    die "Could not read annotated release tag $tag."
  grep -Fq -- '-----BEGIN SSH SIGNATURE-----' <<<"$tag_contents" ||
    die "Release tag $tag is not SSH-signed."
  allowed_signers="$(git -C "$root_dir" config --path --get gpg.ssh.allowedSignersFile 2>/dev/null || true)"
  [[ -n "$allowed_signers" && -r "$allowed_signers" ]] ||
    die "A readable gpg.ssh.allowedSignersFile is required for SSH tag verification."
  exact_signers="$(mktemp)"
  printf '%s\n' 'arsalanahmad.ars@gmail.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIO/nSfxOUA9ltYRu9hxVr5kSk1voTy6hSrxnF99BNAf7' >"$exact_signers"
  if ! git -C "$root_dir" -c gpg.ssh.allowedSignersFile="$exact_signers" verify-tag "$tag" >/dev/null; then
    rm -f "$exact_signers"
    die "SSH signature verification failed for the exact Dakia release key: $tag."
  fi
  rm -f "$exact_signers"
  local_tag_object="$(git -C "$root_dir" rev-parse "refs/tags/$tag")"
  remote_refs="$(git -C "$root_dir" ls-remote --tags origin "refs/tags/$tag" "refs/tags/$tag^{}")"
  remote_tag_object="$(awk -v tag="$tag" '$2 == "refs/tags/" tag { print $1 }' <<<"$remote_refs")"
  remote_tag_commit="$(awk -v tag="$tag" '$2 == "refs/tags/" tag "^{}" { print $1 }' <<<"$remote_refs")"
  [[ "$remote_tag_object" == "$local_tag_object" ]] ||
    die "Remote release tag object $tag does not match the verified local signed tag."
  [[ -n "$remote_tag_commit" ]] || die "Annotated release tag $tag is not present on origin."
  [[ "$remote_tag_commit" == "$head" ]] ||
    die "Remote release tag $tag does not target the exact origin/main commit."
}

verify_local_assets() {
  local artifact checksum_names
  for artifact in "$apple_dmg" "$apple_update" "$apple_signature" "$checksums" "$notes" "$source_marker"; do
    [[ -s "$asset_dir/$artifact" ]] || die "Missing or empty release input: $asset_dir/$artifact"
  done
  [[ "$(wc -l <"$asset_dir/$checksums" | tr -d ' ')" == "3" ]] ||
    die "$checksums must contain exactly the three distributable artifact checksums."
  awk 'NF != 2 { exit 1 }' "$asset_dir/$checksums" ||
    die "$checksums contains an invalid checksum record."
  checksum_names="$(awk '{ print $2 }' "$asset_dir/$checksums" | LC_ALL=C sort)"
  [[ "$checksum_names" == "$(printf '%s\n' "$apple_dmg" "$apple_update" "$apple_signature" | LC_ALL=C sort)" ]] ||
    die "$checksums must cover exactly the GitHub distributable artifacts."
  (cd "$asset_dir" && shasum -a 256 -c "$checksums") >/dev/null ||
    die "$checksums does not match the local release artifacts."
  [[ "$(tr -d '\n' <"$asset_dir/$source_marker")" == "$(git -C "$root_dir" rev-parse HEAD)" ]] ||
    die "$source_marker does not bind these artifacts to the exact main commit."
}

verify_release() {
  local expected_draft="$1" release_json actual_assets actual_tag actual_target actual_title actual_draft actual_prerelease verify_dir artifact
  release_json="$(gh release view "$tag" --repo "$release_repo" --json tagName,targetCommitish,name,body,isDraft,isPrerelease,assets)" ||
    die "GitHub Release $tag does not exist. Prepare and verify its draft before R2 publication."
  actual_tag="$(jq -r '.tagName' <<<"$release_json")"
  actual_target="$(jq -r '.targetCommitish' <<<"$release_json")"
  actual_title="$(jq -r '.name' <<<"$release_json")"
  actual_draft="$(jq -r '.isDraft' <<<"$release_json")"
  actual_prerelease="$(jq -r '.isPrerelease' <<<"$release_json")"
  [[ "$actual_tag" == "$tag" ]] || die "GitHub Release has an unexpected tag."
  [[ "$actual_target" == "$(git -C "$root_dir" rev-parse HEAD)" ]] ||
    die "GitHub Release does not target the exact main commit."
  [[ "$actual_title" == "Dakia $tag" ]] || die "GitHub Release has an unexpected title."
  [[ "$actual_prerelease" == "false" ]] || die "GitHub Release must not be a prerelease."
  actual_assets="$(jq -r '.assets[].name' <<<"$release_json" | LC_ALL=C sort)"
  [[ "$actual_assets" == "$expected_assets" ]] ||
    die "GitHub Release assets do not exactly match the expected four-file allowlist."
  jq -j '.body' <<<"$release_json" >"$asset_dir/.github-release-body.$$"
  if ! cmp -s "$asset_dir/$notes" "$asset_dir/.github-release-body.$$"; then
    rm -f "$asset_dir/.github-release-body.$$"
    die "GitHub Release body does not exactly match release-notes.md."
  fi
  rm -f "$asset_dir/.github-release-body.$$"
  verify_dir="$(mktemp -d)"
  trap 'rm -rf "$verify_dir"' RETURN
  for artifact in "$apple_dmg" "$apple_update" "$apple_signature" "$checksums"; do
    gh release download "$tag" --repo "$release_repo" --dir "$verify_dir" --pattern "$artifact"
    cmp -s "$asset_dir/$artifact" "$verify_dir/$artifact" ||
      die "GitHub Release asset bytes do not match local $artifact."
  done
  (cd "$verify_dir" && shasum -a 256 -c "$checksums") >/dev/null ||
    die "Downloaded GitHub checksum file does not validate the downloaded assets."
  rm -rf "$verify_dir"
  trap - RETURN
  if [[ "$expected_draft" == "true" ]]; then
    [[ "$actual_draft" == "true" ]] || return 2
  else
    [[ "$actual_draft" == "false" ]] ||
      die "GitHub Release draft state is not valid for this stage."
  fi
}

require_public_r2_candidate() {
  local manifest expected_url expected_signature expected_notes status
  manifest="$(mktemp)"
  trap 'rm -f "$manifest"' RETURN
  status="$(curl "${curl_options[@]}" --output "$manifest" --write-out '%{http_code}' \
    "$download_origin/macos/latest/latest.json?release-gate=$tag")"
  [[ "$status" == "200" ]] || die "Public updater manifest is not reachable (HTTP $status)."
  node "$root_dir/scripts/updater-manifest.mjs" validate \
    --manifest "$manifest" \
    --tauri-config "$root_dir/apps/desktop/src-tauri/tauri.conf.json" >/dev/null ||
    die "Public updater manifest does not validate."
  expected_url="$download_origin/macos/$tag/Dakia-aarch64.app.tar.gz"
  expected_signature="$(node -e 'process.stdout.write(require("node:fs").readFileSync(process.argv[1], "utf8").trim())' "$asset_dir/$apple_signature")"
  expected_notes="$(node -e 'process.stdout.write(require("node:fs").readFileSync(process.argv[1], "utf8").trim())' "$asset_dir/$notes")"
  jq -e --arg version "$version" --arg url "$expected_url" --arg signature "$expected_signature" --arg notes "$expected_notes" \
    '.version == $version and .notes == $notes and .platforms["darwin-aarch64"].url == $url and .platforms["darwin-aarch64"].signature == $signature' \
    "$manifest" >/dev/null ||
    die "Public updater manifest is not the exact signed candidate for $tag."
  rm -f "$manifest"
  trap - RETURN
}

verify_public_github_assets() {
  local verify_dir artifact status
  verify_dir="$(mktemp -d)"
  trap 'rm -rf "$verify_dir"' RETURN
  for artifact in "$apple_dmg" "$apple_update" "$apple_signature" "$checksums"; do
    status="$(curl "${curl_options[@]}" --output "$verify_dir/$artifact" --write-out '%{http_code}' \
      "https://github.com/$release_repo/releases/download/$tag/$artifact")"
    [[ "$status" == "200" ]] || die "Public GitHub asset is not reachable: $artifact (HTTP $status)."
    cmp -s "$asset_dir/$artifact" "$verify_dir/$artifact" ||
      die "Public GitHub asset bytes do not match local $artifact."
  done
  (cd "$verify_dir" && shasum -a 256 -c "$checksums") >/dev/null ||
    die "Public GitHub checksum file does not validate downloaded assets."
  rm -rf "$verify_dir"
  trap - RETURN
}

require_main_provenance
require_exact_signed_tag
verify_local_assets
gh auth status --hostname github.com >/dev/null || die "GitHub authentication for github.com is required."
if verify_release true; then
  require_public_r2_candidate
  dakia_require_release_mutation_provenance "$root_dir" || exit 1
  gh release edit "$tag" --repo "$release_repo" --draft=false --latest
else
  release_status=$?
  [[ "$release_status" -eq 2 ]] || die "GitHub Release draft verification failed."
  require_public_r2_candidate
  verify_release false
fi
verify_release false
verify_public_github_assets
echo "Published and independently verified GitHub Release for $tag."
