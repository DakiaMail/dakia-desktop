# Publishing a macOS Release

Dakia releases are built, signed, notarized, and published locally from the
primary Apple Silicon Mac. R2 is the public artifact host. The release path is
Apple Silicon only; GitHub Actions and GitHub Releases are not used.

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
2. verifies packaged executable, classifier resources, and legal notices;
3. runs the packaged-app startup check on the release Mac;
4. notarizes and staples the app;
5. rebuilds, notarizes, staples, mounts, and verifies the DMG;
6. archives that same final app and signs the exact archive bytes for Tauri.

The publisher uses immutable versioned R2 paths, anonymously verifies the
published bytes, validates the updater manifest, and publishes `latest.json`
only after all artifact checks pass.

For updater key custody and artifact details, see
[Signed Desktop Updates](updater-release.md).
