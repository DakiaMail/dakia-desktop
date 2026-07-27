# Releasing

Run all release work locally:

```bash
npm run verify:local
```

For macOS, the primary Apple Silicon Mac builds the Apple Silicon release. R2
hosts the versioned DMG, signed updater archive, stable download alias, and
updater manifest.

GitHub Actions and GitHub Releases are not part of this process. A Git tag is an
optional source-control marker only.

Follow:

- [Publishing A macOS Release](publishing-macos-release.md) for local signing,
  notarization, and publication;
- [Signed Desktop Updates](updater-release.md) for key custody and the
  production publication gate.
