# Releasing

Run all release work locally:

```bash
npm run verify:local
```

For macOS, the primary Apple Silicon Mac builds the Apple Silicon release. R2
hosts the versioned DMG, signed updater archive, stable download alias, and
updater manifest. A GitHub Release mirrors those same locally built assets for
public downloads.

GitHub Actions are not part of this process. Create the GitHub Release only
after R2 publication is verified, using the signed source tag and the exact
local artifacts produced by the release builder.

Follow:

- [Publishing A macOS Release](publishing-macos-release.md) for local signing,
  notarization, and publication;
- [Signed Desktop Updates](updater-release.md) for key custody and the
  production publication gate.
