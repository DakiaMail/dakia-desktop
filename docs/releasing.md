# Releasing

Run all release work locally:

```bash
npm run verify:local
```

For macOS, the primary Apple Silicon Mac builds both architectures. A separate
Intel MacBook performs native Intel startup and all three updater acceptance
modes. R2 hosts versioned DMGs, signed updater archives, stable download aliases,
and the updater manifest.

GitHub Actions and GitHub Releases are not part of this process. A Git tag is an
optional source-control marker only.

Follow:

- [Publishing A macOS Release](publishing-macos-release.md) for local signing,
  notarization, and publication;
- [Signed Desktop Updates](updater-release.md) for key custody, staging,
  two-machine acceptance evidence, and the production feed gate.
