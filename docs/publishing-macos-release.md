# Publishing a macOS Release

The normal production release path is the GitHub-hosted
`production-release` workflow, scheduled for 02:17 UTC. It builds the macOS
Apple Silicon release on `macos-15` after the complete verification suite has
passed, imports the Developer ID identity, notarizes and staples the release,
and publishes it alongside the Linux x64 and Windows x64 artifacts. It runs
only from an exact live `origin/main` commit and creates an annotated
SSH-signed source tag before publication.

The macOS updater remains R2-hosted at:

```text
https://downloads.dakiamail.com/macos/latest/latest.json
```

The workflow uploads immutable release objects beneath
`https://downloads.dakiamail.com/macos/vX.Y.Z/`, writes `latest.json` only
after the candidate has passed its checks, and makes the matching GitHub
Release public only after R2 is publicly converged. The final workflow job
performs anonymous public verification; a failed verification is not a
successful release.

## Manual Apple Silicon fallback

Use this path only for an exceptional manual recovery/release on a trusted
Apple Silicon Mac. It produces the macOS part of a release; the hosted
workflow is the normal cross-platform producer. Start from a clean local
`main` that exactly matches cached and live `origin/main` and has the required
Developer ID identity, `dakia-notary` Keychain profile, updater signing key,
Google Desktop OAuth values, R2 credentials, GitHub authentication, and the
trusted SSH tag-signing key.

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

`release:build` signs, notarizes, staples, and verifies the final application
and DMG before it creates and Tauri-signs the updater archive. The local
publisher refuses to replace immutable versioned objects, anonymously
byte-verifies uploads, validates the updater manifest, and publishes the feed
last. The GitHub scripts require an exact source tag and release assets; they
create and verify a draft before R2 mutation, then publish that verified draft
only after R2 confirms the candidate. They do not clobber assets or replace a
release on mismatch.

The locally built macOS asset directory contains the Apple Silicon DMG,
updater archive and detached signature, release notes, checksums, and source
commit marker. Consult [Signed Desktop Updates](updater-release.md) for the
cross-platform artifact layout and key-custody requirements.
