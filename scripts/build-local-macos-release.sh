#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -lt 1 || "$#" -gt 2 ]]; then
  echo "Usage: $0 vX.Y.Z [/path/to/output]" >&2
  exit 2
fi

tag="$1"
output_dir="${2:-}"
root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ ! "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ || \
      "$output_dir" == "staging" || "$output_dir" == "production" ]]; then
  echo "Usage: $0 vX.Y.Z [/path/to/output]" >&2
  exit 2
fi
version="${tag#v}"
output_dir="${output_dir:-$root_dir/release-assets/$tag}"
package_version="$(node -p "require('$root_dir/package.json').version")"
cargo_version="$(awk '
  /^\[workspace\.package\]$/ { in_workspace_package = 1; next }
  /^\[/ { in_workspace_package = 0 }
  in_workspace_package && /^version = / { gsub(/"/, "", $3); print $3; exit }
' "$root_dir/Cargo.toml")"
tauri_version="$(node -p "require('$root_dir/apps/desktop/src-tauri/tauri.conf.json').version")"
package_lock_matches="$(node -e '
  const lock = require(process.argv[1]);
  const expected = process.argv[2];
  process.stdout.write(String(
    lock.version === expected && lock.packages?.[""]?.version === expected
  ));
' "$root_dir/package-lock.json" "$version")"
release_notes_source="$root_dir/docs/releases/$tag.md"
lock_versions="$(awk '
  /^name = "dakia-(core|cli|desktop)"$/ {
    package_name = $3
    gsub(/"/, "", package_name)
    read_version = 1
    next
  }
  read_version && /^version = / {
    gsub(/"/, "", $3)
    print package_name "=" $3
    package_name = ""
    read_version = 0
  }
' "$root_dir/Cargo.lock" | sort -u)"
expected_lock_versions="$(printf '%s\n' \
  "dakia-cli=$version" \
  "dakia-core=$version" \
  "dakia-desktop=$version")"
if [[ "$version" != "$package_version" || "$version" != "$cargo_version" || \
      "$version" != "$tauri_version" || \
      "$lock_versions" != "$expected_lock_versions" || \
      "$package_lock_matches" != "true" ]]; then
  echo "Tag, package, package lock, Cargo workspace, Tauri config, and workspace lock versions must match $tag." >&2
  exit 1
fi
if [[ "$(git -C "$root_dir" branch --show-current)" != "main" ]]; then
  echo "Release source must be built from main." >&2
  exit 1
fi
source_commit="$(git -C "$root_dir" rev-parse HEAD)"
origin_main_commit="$(git -C "$root_dir" rev-parse origin/main)"
if [[ "$source_commit" != "$origin_main_commit" ]]; then
  echo "Release source must exactly match the fetched origin/main." >&2
  exit 1
fi
if [[ -n "$(git -C "$root_dir" status --porcelain=v1 --untracked-files=all)" ]]; then
  echo "Release source has tracked or untracked changes; commit or remove them before building." >&2
  exit 1
fi
if ! git -C "$root_dir" ls-files --error-unmatch -- \
  "docs/releases/$tag.md" >/dev/null 2>&1 ||
  [[ ! -s "$release_notes_source" ]]; then
  echo "Missing tracked release notes: $release_notes_source" >&2
  exit 1
fi
if [[ "$(uname -m)" != "arm64" ]]; then
  echo "The Apple Silicon builder must run on the primary Apple Silicon release Mac." >&2
  exit 1
fi

# shellcheck source=local-release-env.sh
source "$root_dir/scripts/local-release-env.sh"
dakia_require_google_oauth_environment
dakia_require_signing_environment

outputs=(
  "Dakia_${version}_aarch64.dmg"
  "Dakia-aarch64.app.tar.gz"
  "Dakia-aarch64.app.tar.gz.sig"
)
local_release_files=("${outputs[@]}" "release-notes.md" "SHA256SUMS.txt" "source-commit.txt")
for filename in "${local_release_files[@]}"; do
  if [[ -e "$output_dir/$filename" ]]; then
    echo "Refusing to overwrite existing local release artifact: $output_dir/$filename" >&2
    exit 1
  fi
done
mkdir -p "$output_dir"

cd "$root_dir"
npm run setup:worktree
npm run prepare:desktop-assets
# Run every JavaScript preparation step without OAuth material. Tauri normally
# invokes this from its beforeBuildCommand, so the release-only override below
# prevents the secret-bearing compiler invocation from re-running it.
npm run build:desktop-web
release_tauri_config="$(mktemp "${TMPDIR:-/tmp}/dakia-release-tauri-config.XXXXXX")"
printf '%s\n' '{"build":{"beforeBuildCommand":""}}' >"$release_tauri_config"
dakia_prepare_google_oauth_compiler_environment
trap 'dakia_clear_google_oauth_compiler_environment; rm -f "$release_tauri_config"' EXIT HUP INT TERM

ORT_LIB_LOCATION="$root_dir/apps/desktop/src-tauri/frameworks" \
ORT_PREFER_DYNAMIC_LINK=1 \
TAURI_ENV_ARCH=aarch64 \
TAURI_ENV_PLATFORM=macos \
  npm run bundle:cli

# Cargo cannot observe changes to the release-only secret file consumed by the
# compiler wrapper. Rebuild the desktop package so the current credential is
# always compiled into the artifact that the release verifier exercises.
cargo clean -p dakia-desktop --target aarch64-apple-darwin

ORT_LIB_LOCATION="$root_dir/apps/desktop/src-tauri/frameworks" \
ORT_PREFER_DYNAMIC_LINK=1 \
  "$root_dir/node_modules/.bin/tauri" build \
    --target aarch64-apple-darwin \
    --config apps/desktop/src-tauri/tauri.conf.json \
    --config "$release_tauri_config"

app="$root_dir/target/aarch64-apple-darwin/release/bundle/macos/Dakia.app"
"$root_dir/scripts/sign-macos-release-app.sh" "$app" "$APPLE_SIGNING_IDENTITY"
"$root_dir/scripts/verify-macos-release-app.sh" "$app"

notary_dir="$(mktemp -d "${TMPDIR:-/tmp}/dakia-app-notary.XXXXXX")"
app_zip="$notary_dir/Dakia-aarch64.zip"
trap 'dakia_clear_google_oauth_compiler_environment; rm -f "$release_tauri_config"; rm -rf "$notary_dir"' EXIT HUP INT TERM
ditto -c -k --sequesterRsrc --keepParent "$app" "$app_zip"
xcrun notarytool submit "$app_zip" --keychain-profile "$APPLE_NOTARY_PROFILE" --wait
rm -rf "$notary_dir"
xcrun stapler staple "$app"
codesign --verify --deep --strict --verbose=2 "$app"
spctl --assess --type execute --verbose=2 "$app"
xcrun stapler validate "$app"

dmg="$output_dir/Dakia_${version}_aarch64.dmg"
"$root_dir/scripts/rebuild-notarized-dmg.sh" "$version" "$dmg"
"$root_dir/scripts/verify-macos-release-dmg.sh" "$dmg"
"$root_dir/scripts/package-macos-updater.sh" "$version" "$output_dir"

cp "$release_notes_source" "$output_dir/release-notes.md"
for filename in "${outputs[@]}"; do
  [[ -s "$output_dir/$filename" ]] || {
    echo "Release build is missing: $filename" >&2
    exit 1
  }
done
(cd "$output_dir" && shasum -a 256 "${outputs[@]}" > SHA256SUMS.txt)
printf '%s\n' "$source_commit" >"$output_dir/source-commit.txt"
if [[ "$(git -C "$root_dir" branch --show-current)" != "main" || \
      "$(git -C "$root_dir" rev-parse HEAD)" != "$source_commit" || \
      "$(git -C "$root_dir" rev-parse origin/main)" != "$source_commit" || \
      -n "$(git -C "$root_dir" status --porcelain=v1 --untracked-files=all)" ]]; then
  echo "Release source changed while artifacts were being built." >&2
  exit 1
fi
echo "Built signed, notarized Apple Silicon artifacts in $output_dir"
