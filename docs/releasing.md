# Releasing

The primary production path is the GitHub-hosted `production-release` workflow.
It runs nightly at 02:17 UTC from the exact live `origin/main` state, or may be
started manually with a reason and an explicit force option. A no-change run
stops after its inexpensive decision job.

When a release is eligible, the workflow prepares a patch release if necessary,
runs the complete source verification suite, and only then starts any platform
build. In particular, the full suite is a gate before the Apple Silicon build.
It builds:

- macOS Apple Silicon on `macos-15`, signed and notarized;
- Linux x64 AppImage on `ubuntu-24.04`;
- Windows x64 NSIS installer on `windows-2025` (signed when the Windows
  certificate is configured).

The job creates or verifies an annotated SSH-signed `vX.Y.Z` tag pointing at
the exact release commit before it stages the GitHub Release. It publishes
immutable versioned R2 objects under
`https://downloads.dakiamail.com/{macos,linux,windows}/vX.Y.Z/`, moves each
platform's `latest/latest.json` only after its candidate is verified, and makes
the GitHub Release public only after R2 has converged. A final anonymous job
checks the three feeds, published artifacts, provenance tag, and GitHub mirror.

The installed application polls its platform-specific updater feed:

```text
https://downloads.dakiamail.com/macos/latest/latest.json
https://downloads.dakiamail.com/linux/latest/latest.json
https://downloads.dakiamail.com/windows/latest/latest.json
```

For an exceptional manual macOS recovery or release, retain the local Apple
Silicon flow in [Publishing a macOS Release](publishing-macos-release.md). It
uses the same provenance, artifact-verification, R2, and GitHub-release gates;
it is a fallback, not the normal nightly path.

See [Signed Desktop Updates](updater-release.md) for updater artifacts, key
custody, and publication guarantees.
