# Releasing

Nightly releases run automatically from `main` through the local Codex
automation on the trusted Apple Silicon release Mac. They skip when there are
no commits after the public updater release.

For an exceptional manual release, run the same work on that runner:

```bash
npm run verify:local
```

For macOS, the primary Apple Silicon Mac builds the Apple Silicon release. R2
hosts the versioned DMG, signed updater archive, stable download alias, and
updater manifest. A GitHub Release mirrors those same locally built assets for
public downloads.

The local Codex automation first creates and downloads-verifies an exact GitHub
Release draft from the signed source tag and locally built artifacts. It then
publishes the immutable R2 artifacts and `latest.json`. Only after the public
updater manifest exactly matches that candidate does it make the verified GitHub
draft public and independently byte-compare the public GitHub downloads.

Follow:

- [Publishing A macOS Release](publishing-macos-release.md) for local signing,
  notarization, and publication;
- [Signed Desktop Updates](updater-release.md) for key custody and the
  production publication gate.
