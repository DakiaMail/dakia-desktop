#!/usr/bin/env bash
set -euo pipefail

tag="${1:-}"
channel="${2:-}"
output_dir="${3:-}"
arch_filter="${4:-both}"
root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ ! "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ || \
      ( "$channel" != "staging" && "$channel" != "production" ) || \
      ( "$arch_filter" != "both" && "$arch_filter" != "aarch64" && \
        "$arch_filter" != "x86_64" ) ]]; then
  echo "Usage: $0 vX.Y.Z <staging|production> [/path/to/output] [both|aarch64|x86_64]" >&2
  exit 2
fi
version="${tag#v}"
output_dir="${output_dir:-$root_dir/release-assets/$tag/$channel}"
package_version="$(node -p "require('$root_dir/package.json').version")"
if [[ "$version" != "$package_version" ]]; then
  echo "Tag $tag does not match package version $package_version." >&2
  exit 1
fi
if [[ "$(uname -m)" != "arm64" ]]; then
  echo "The dual-architecture builder is intended to run on the primary Apple Silicon release Mac." >&2
  exit 1
fi

# shellcheck source=local-release-env.sh
source "$root_dir/scripts/local-release-env.sh"
dakia_require_google_oauth_environment
dakia_require_signing_environment

all_outputs=(
  "Dakia_${version}_aarch64.dmg"
  "Dakia_${version}_x64.dmg"
  "Dakia-aarch64.app.tar.gz"
  "Dakia-aarch64.app.tar.gz.sig"
  "Dakia-x86_64.app.tar.gz"
  "Dakia-x86_64.app.tar.gz.sig"
)
case "$arch_filter" in
  both)
    descriptors=(
      "aarch64-apple-darwin:aarch64:aarch64"
      "x86_64-apple-darwin:x86_64:x64"
    )
    selected_outputs=("${all_outputs[@]}")
    ;;
  aarch64)
    descriptors=("aarch64-apple-darwin:aarch64:aarch64")
    selected_outputs=(
      "Dakia_${version}_aarch64.dmg"
      "Dakia-aarch64.app.tar.gz"
      "Dakia-aarch64.app.tar.gz.sig"
    )
    ;;
  x86_64)
    descriptors=("x86_64-apple-darwin:x86_64:x64")
    selected_outputs=(
      "Dakia_${version}_x64.dmg"
      "Dakia-x86_64.app.tar.gz"
      "Dakia-x86_64.app.tar.gz.sig"
    )
    ;;
esac
for filename in "${selected_outputs[@]}"; do
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

for descriptor in "${descriptors[@]}"; do
  IFS=: read -r target arch dmg_suffix <<<"$descriptor"
  ORT_LIB_LOCATION="$root_dir/apps/desktop/src-tauri/frameworks" \
  ORT_PREFER_DYNAMIC_LINK=1 \
  TAURI_ENV_ARCH="$arch" \
  TAURI_ENV_PLATFORM=macos \
    npm run bundle:cli
  config_args=(
    --target "$target"
    --config apps/desktop/src-tauri/tauri.conf.json
    --config "$release_tauri_config"
  )
  if [[ "$channel" == "staging" ]]; then
    config_args+=(--config apps/desktop/src-tauri/tauri.staging-updater.conf.json)
  fi

  ORT_LIB_LOCATION="$root_dir/apps/desktop/src-tauri/frameworks" \
  ORT_PREFER_DYNAMIC_LINK=1 \
    "$root_dir/node_modules/.bin/tauri" build "${config_args[@]}"

  bundle_root="$root_dir/target/$target/release/bundle"
  app="$bundle_root/macos/Dakia.app"
  "$root_dir/scripts/sign-macos-release-app.sh" \
    "$app" "$APPLE_SIGNING_IDENTITY"

  "$root_dir/scripts/verify-macos-release-app.sh" --static-only "$app"
  if file "$app/Contents/MacOS/dakia-desktop" | grep -q "$(uname -m)"; then
    "$root_dir/scripts/verify-macos-release-app.sh" "$app"
  else
    echo "Static app/resource/legal verification passed; native startup will be exercised on the Intel MacBook: $target"
  fi

  notary_dir="$(mktemp -d "${TMPDIR:-/tmp}/dakia-app-notary.XXXXXX")"
  app_zip="$notary_dir/Dakia-$arch.zip"
  ditto -c -k --sequesterRsrc --keepParent "$app" "$app_zip"
  xcrun notarytool submit "$app_zip" \
    --keychain-profile "$APPLE_NOTARY_PROFILE" --wait
  rm -rf "$notary_dir"
  xcrun stapler staple "$app"
  codesign --verify --deep --strict --verbose=2 "$app"
  spctl --assess --type execute --verbose=2 "$app"
  xcrun stapler validate "$app"

  dmg="$output_dir/Dakia_${version}_${dmg_suffix}.dmg"
  "$root_dir/scripts/rebuild-notarized-dmg.sh" \
    "$dmg_suffix" "$version" "$dmg"
  "$root_dir/scripts/verify-macos-release-dmg.sh" "$dmg"
  "$root_dir/scripts/package-macos-updater.sh" \
    "$arch" "$version" "$output_dir"
done

if [[ ! -s "$output_dir/release-notes.md" ]]; then
  printf 'Dakia %s\n' "$tag" >"$output_dir/release-notes.md"
fi
for filename in "${all_outputs[@]}"; do
  [[ -s "$output_dir/$filename" ]] || {
    echo "Architecture build is complete, but the combined release set still needs: $filename"
    exit 0
  }
done
(cd "$output_dir" && shasum -a 256 "${all_outputs[@]}" > SHA256SUMS.txt)
echo "Built signed, notarized $channel artifacts in $output_dir"
