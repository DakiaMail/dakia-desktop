# Publishing a macOS Release

Dakia releases are built, signed, notarized, and published locally from the
primary Apple Silicon Mac. R2 remains the production updater host. A GitHub
Release mirrors the locally built assets for public downloads; GitHub Actions
does not build or publish them. The release path is Apple Silicon only.

Before a release, ensure the intended code is present and the version matches
`package.json`, the Cargo workspace, and `apps/desktop/src-tauri/tauri.conf.json`.
The release commands validate their required signing, notarization, OAuth, and
R2 credentials directly, so there is no separate manual preflight checklist.

## Commands

```bash
npm run verify:local
npm run release:build -- vX.Y.Z
npm run release:publish -- vX.Y.Z "$PWD/release-assets/vX.Y.Z"
```

The verification command runs Rust formatting, Clippy, Rust tests, TypeScript,
formatting, frontend tests, release-script tests, and the frontend build. It no
longer builds a separate packaged app: the release builder verifies the actual
signed final artifact instead.

The builder:

1. assembles, Developer ID signs, and verifies the Apple Silicon app;
2. verifies the packaged app and CLI executables, Apple-Silicon architecture,
   matching Developer ID ownership/version, classifier resources, legal
   notices, and compiled Google OAuth configuration without runtime overrides;
3. runs isolated packaged CLI and app startup checks on the release Mac;
4. notarizes and staples the app;
5. rebuilds, notarizes, staples, mounts, and verifies the DMG;
6. archives that same final app, extracts and repeats the app/CLI acceptance
   checks on the archive contents, then signs the exact archive bytes for Tauri.

The publisher re-verifies the DMG, cryptographically verifies and extracts the
updater archive, checks the archived app version and packaged-app acceptance,
uses immutable versioned R2 paths, anonymously verifies the published bytes,
validates the updater manifest, and publishes `latest.json` only after all
artifact checks pass.

After R2 publication is proven, create the signed source tag and a draft GitHub
Release. Use `release-assets/vX.Y.Z/release-notes.md` as the release body and
upload the DMG, updater archive, detached signature, and `SHA256SUMS.txt` from
that same local directory. Verify the uploaded assets against the local
checksums before publishing the draft. GitHub is a download mirror; the Tauri
manifest continues to reference immutable R2 URLs.

Use an explicit repository and refuse an unexpected existing Release:

```bash
release_repo="DakiaMail/dakia-desktop"
release_tag="vX.Y.Z"
release_dir="$PWD/release-assets/$release_tag"

test "$(git rev-parse "$release_tag^{commit}")" = "$(git rev-parse HEAD)"
if gh release view "$release_tag" --repo "$release_repo" >/dev/null 2>&1; then
  echo "Refusing to replace an existing GitHub Release: $release_tag" >&2
  exit 1
fi

gh release create "$release_tag" \
  --repo "$release_repo" \
  --verify-tag \
  --draft \
  --latest \
  --title "Dakia $release_tag" \
  --notes-file "$release_dir/release-notes.md" \
  "$release_dir/Dakia_${release_tag#v}_aarch64.dmg" \
  "$release_dir/Dakia-aarch64.app.tar.gz" \
  "$release_dir/Dakia-aarch64.app.tar.gz.sig" \
  "$release_dir/SHA256SUMS.txt"
```

Before publishing the draft, require the exact four-file allowlist, compare the
release body, download the draft assets through GitHub, and verify their bytes:

```bash
expected_assets="$(
  printf '%s\n' \
    "Dakia_${release_tag#v}_aarch64.dmg" \
    "Dakia-aarch64.app.tar.gz" \
    "Dakia-aarch64.app.tar.gz.sig" \
    "SHA256SUMS.txt" |
    sort
)"
actual_assets="$(
  gh release view "$release_tag" \
    --repo "$release_repo" \
    --json assets \
    --jq '.assets[].name' |
    sort
)"
test "$actual_assets" = "$expected_assets"
gh release view "$release_tag" \
  --repo "$release_repo" \
  --json body \
  --jq .body |
  cmp - "$release_dir/release-notes.md"

github_verify_dir="$(mktemp -d)"
gh release download "$release_tag" \
  --repo "$release_repo" \
  --dir "$github_verify_dir"
cmp "$release_dir/SHA256SUMS.txt" "$github_verify_dir/SHA256SUMS.txt"
(
  cd "$github_verify_dir"
  shasum -a 256 -c SHA256SUMS.txt
)
```

Only then publish the GitHub Release:

```bash
gh release edit "$release_tag" \
  --repo "$release_repo" \
  --draft=false \
  --latest
```

Finally, anonymously download each public asset from
`https://github.com/DakiaMail/dakia-desktop/releases/download/vX.Y.Z/` and
compare it byte-for-byte with the corresponding local file. If an earlier
attempt left a draft, do not use `--clobber`: download and compare its body and
exact asset allowlist. Continue only if everything matches; otherwise stop and
remove or replace the draft only with separate approval.

For updater key custody and artifact details, see
[Signed Desktop Updates](updater-release.md).
