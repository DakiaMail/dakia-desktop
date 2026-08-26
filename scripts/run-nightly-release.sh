#!/usr/bin/env bash
set -euo pipefail

# This script deliberately runs only on the trusted Apple Silicon release
# runner. It keeps source, updater, and GitHub release publication in one
# serialized transaction and leaves immutable artifacts untouched on retries.
root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
repo="DakiaMail/dakia-desktop"
cd "$root_dir"

test "$(git branch --show-current)" = main
git fetch --force --tags origin main
test "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)"
test "$(git status --porcelain)" = ""
test "$(uname -m)" = arm64

latest_manifest="$(curl --fail --silent --show-error --location https://downloads.dakiamail.com/macos/latest/latest.json)"
latest_version="$(jq -er '.version | select(test("^[0-9]+\\.[0-9]+\\.[0-9]+$"))' <<<"$latest_manifest")"
latest_tag="v$latest_version"

# A source tag is preferred. v0.4.0 predates its source tag, so retain a
# narrow migration fallback based on the tracked release note that fed the
# published updater manifest. Do not guess from dates or commit messages.
if git rev-parse --verify --quiet "$latest_tag^{commit}" >/dev/null; then
  release_commit="$(git rev-parse "$latest_tag^{commit}")"
else
  release_commit="$(git log -1 --format=%H -- "docs/releases/$latest_tag.md")"
  test -n "$release_commit" || {
    echo "The published updater $latest_tag has no source tag or tracked release note." >&2
    exit 1
  }
fi
git merge-base --is-ancestor "$release_commit" HEAD

if test -z "$(git log --format=%H "$release_commit..HEAD")"; then
  echo "No source changes since $latest_tag; nightly release skipped."
  exit 0
fi

current_version="$(node -p "require('./package.json').version")"
if test "$current_version" = "$latest_version"; then
  prepared="$(node scripts/prepare-nightly-release.mjs "$release_commit")"
  tag="$(jq -er .tag <<<"$prepared")"
  notes="$(jq -er .notes <<<"$prepared")"

  # Verify before publishing the release-preparation commit. A verification
  # failure is therefore retryable on the next nightly run without consuming a
  # version number or leaving main in a half-prepared state.
  npm run verify:local

  git add -- package.json package-lock.json Cargo.toml Cargo.lock \
    apps/desktop/src-tauri/tauri.conf.json "$notes"
  git diff --cached --check
  git commit -m "Prepare $tag nightly release"
  git push origin HEAD:main
else
  tag="v$current_version"
  notes="docs/releases/$tag.md"
  test -s "$notes" || {
    echo "Unreleased source version $tag is missing its tracked release notes." >&2
    exit 1
  }
fi

test "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)"
npm run release:build -- "$tag"
release_dir="$root_dir/release-assets/$tag"
npm run release:publish -- "$tag" "$release_dir"

# Tag only after the updater is publicly available. This prevents a failed
# build from making an unreleasable source revision look published.
if git rev-parse --verify --quiet "$tag^{commit}" >/dev/null; then
  test "$(git rev-parse "$tag^{commit}")" = "$(git rev-parse HEAD)"
else
  test -n "$(git config --get user.signingkey)" || {
    echo "The trusted release runner needs a configured Git signing key." >&2
    exit 1
  }
  git tag -s "$tag" -m "Dakia $tag"
  git push origin "$tag"
fi

if gh release view "$tag" --repo "$repo" >/dev/null 2>&1; then
  echo "Refusing to replace an existing GitHub Release: $tag" >&2
  exit 1
fi
gh release create "$tag" --repo "$repo" --verify-tag --draft --latest \
  --title "Dakia $tag" --notes-file "$release_dir/release-notes.md" \
  "$release_dir/Dakia_${tag#v}_aarch64.dmg" \
  "$release_dir/Dakia-aarch64.app.tar.gz" \
  "$release_dir/Dakia-aarch64.app.tar.gz.sig" \
  "$release_dir/SHA256SUMS.txt"

expected_assets="$(printf '%s\n' "Dakia_${tag#v}_aarch64.dmg" Dakia-aarch64.app.tar.gz Dakia-aarch64.app.tar.gz.sig SHA256SUMS.txt | sort)"
actual_assets="$(gh release view "$tag" --repo "$repo" --json assets --jq '.assets[].name' | sort)"
test "$actual_assets" = "$expected_assets"
gh release view "$tag" --repo "$repo" --json body --jq .body | cmp - "$release_dir/release-notes.md"
verify_dir="$(mktemp -d)"
trap 'rm -rf "$verify_dir"' EXIT
gh release download "$tag" --repo "$repo" --dir "$verify_dir"
cmp "$release_dir/SHA256SUMS.txt" "$verify_dir/SHA256SUMS.txt"
(cd "$verify_dir" && shasum -a 256 -c SHA256SUMS.txt)
gh release edit "$tag" --repo "$repo" --draft=false --latest

echo "Published $tag from $(git rev-parse HEAD)."
