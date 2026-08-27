# Publishing a macOS Release

Dakia releases are built, signed, notarized, and published from the trusted
Apple Silicon release runner. R2 remains the production updater host. A GitHub
Release mirrors the locally built assets for public downloads. The release path
is Apple Silicon only.

Every night at 02:17 local time, a Codex automation runs the release path on
the primary Apple Silicon Mac from `main`, but only if `main` contains commits
after the version in the public updater manifest. The local release Mac must
have its Keychain identities, notarization profile, updater key, and R2
credentials available. It creates the next patch version, writes a concise
user-facing release note, verifies, publishes to R2, and then publishes the
matching GitHub Release.

Before a release, ensure the intended code is present and the version matches
`package.json`, the Cargo workspace, and `apps/desktop/src-tauri/tauri.conf.json`.
The release commands validate their required signing, notarization, OAuth, and
R2 credentials directly, so there is no separate manual preflight checklist.

## Commands

```bash
npm run verify:local
npm run release:build -- vX.Y.Z
git tag -s vX.Y.Z -m "Dakia vX.Y.Z"
git verify-tag vX.Y.Z
git push origin refs/tags/vX.Y.Z
npm run release:github:draft -- vX.Y.Z "$PWD/release-assets/vX.Y.Z"
npm run release:publish -- vX.Y.Z "$PWD/release-assets/vX.Y.Z"
npm run release:github:publish -- vX.Y.Z "$PWD/release-assets/vX.Y.Z"
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

## GitHub mirror order

GitHub is a public download mirror, not the updater host. Create and verify its
draft before any R2 mutation, so the exact signed local artifacts have already
been checked by both services before the updater feed can move:

```bash
release_tag="vX.Y.Z"
release_dir="$PWD/release-assets/$release_tag"
git tag -s "$release_tag" -m "Dakia $release_tag"
git verify-tag "$release_tag"
git push origin "refs/tags/$release_tag"
npm run release:github:draft -- "$release_tag" "$release_dir"
npm run release:publish -- "$release_tag" "$release_dir"
npm run release:github:publish -- "$release_tag" "$release_dir"
```

The draft stage requires a clean local `main` equal to `origin/main`, the
explicit `DakiaMail/dakia-desktop` origin, a pushed annotated SSH-signed tag
whose commit exactly equals `HEAD`, and working GitHub authentication. It
requires exactly the DMG, updater archive, detached signature, and
`SHA256SUMS.txt`; verifies the local checksum file; creates a draft targeted at
the exact commit; then downloads every draft asset and compares it byte-for-byte
with the local input. The release title and body must also exactly match the
candidate (`Dakia vX.Y.Z` and `release-notes.md`).

Only after the R2 publisher has anonymously exposed an exact updater manifest
for that version, archive URL, and updater signature may the final GitHub stage
make the draft public. It rechecks every draft property and asset first, then
downloads all four public GitHub assets and validates both their bytes and their
downloaded `SHA256SUMS.txt`.

Both stages are safe to retry: an existing GitHub draft or already-public
release is accepted only after the full exact comparison succeeds. They never
use `--clobber`, delete a release, or replace assets. A mismatch is a hard stop;
remediation or removal requires separate approval.

The R2 publisher also invokes the draft verifier itself before its first remote
mutation. This makes the required ordering fail closed even if an operator
calls `release:publish` directly.

For updater key custody and artifact details, see
[Signed Desktop Updates](updater-release.md).
