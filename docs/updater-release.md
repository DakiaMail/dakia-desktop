# Signed Desktop Updates

Dakia's hosted production workflow publishes one signed updater candidate for
each supported platform: macOS Apple Silicon, Linux x64, and Windows x64. The
installed app downloads its own platform feed over HTTPS and Tauri verifies the
artifact with the embedded updater public key before an update is installed.
GitHub Releases are public mirrors, never the updater host.

## Feeds and versioned objects

Each platform has an independent feed and immutable versioned-object prefix:

| Platform | Feed | Versioned prefix |
| --- | --- | --- |
| macOS Apple Silicon | `macos/latest/latest.json` | `macos/vX.Y.Z/` |
| Linux x64 | `linux/latest/latest.json` | `linux/vX.Y.Z/` |
| Windows x64 | `windows/latest/latest.json` | `windows/vX.Y.Z/` |

All paths are rooted at `https://downloads.dakiamail.com/`. The feeds carry
the same release version, but only the single updater platform appropriate to
that operating system. Versioned artifacts are immutable; `latest.json` is the
only moving updater pointer and is written after the platform candidate is
fully checked.

The release artifacts are:

- macOS: `Dakia_<version>_aarch64.dmg`, `Dakia-aarch64.app.tar.gz`, and
  `Dakia-aarch64.app.tar.gz.sig`;
- Linux: `Dakia_<version>_amd64.AppImage` and its `.sig`;
- Windows: `Dakia_<version>_x64-setup.exe` and its `.sig`.

Each platform directory also has a `SHA256SUMS.txt` for its distributable
files. macOS release inputs additionally contain `release-notes.md` and
`source-commit.txt` for publication/provenance checks.

## Hosted release gate

`production-release` first decides whether commits exist after the current
public version. If a version bump is needed, it prepares and pushes it to
`main`, then checks out that exact pushed commit for every later job. The
complete Rust and frontend verification suite must pass before any platform
build begins. The macOS builder signs, notarizes, staples, and verifies its
final updater archive; Linux and Windows build their native updater installers
with platform-specific feed configuration. Windows signing is conditional on
the configured Windows certificate.

Before external publication, the workflow creates or verifies an annotated
SSH-signed `vX.Y.Z` tag that points exactly to the release commit. It stages a
GitHub Release, publishes and anonymously verifies immutable R2 objects and
all three manifests, then makes the verified GitHub Release public. An
anonymous final job checks manifests, checksums, updater signatures,
versioned-object bytes, tag provenance, and GitHub Release assets. Any failed
gate leaves the workflow unsuccessful and does not assert a completed release.

## Key custody and manual fallback

The permanent updater public key is embedded in
`apps/desktop/src-tauri/tauri.conf.json`; its private counterpart must remain
restricted to the production release environment and the trusted local
Apple Silicon fallback machine. The hosted macOS job also needs its Developer
ID certificate and notarization API credentials. Store only the necessary
values as environment-scoped GitHub secrets; never commit signing keys,
certificate material, OAuth client secrets, or R2 credentials.

For an exceptional manual macOS release, use the commands and preconditions in
[Publishing a macOS Release](publishing-macos-release.md). That fallback must
keep the same signed-tag provenance and GitHub-draft → R2 → public-GitHub
ordering. It does not replace the hosted cross-platform nightly path.
