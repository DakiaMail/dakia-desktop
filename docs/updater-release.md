# Signed Desktop Updates

Dakia publishes signed Apple Silicon updates through Tauri's native updater.
The installed app downloads over the native transport, verifies the archive
with its embedded public key, and installs only after the user chooses
**Install and Restart**. GitHub Releases mirror the locally built assets for
public downloads, but they do not replace the R2 updater feed. GitHub Actions,
staging channels, and Intel builds are not part of this release path.

## Local trust material

The permanent updater public key is embedded in
`apps/desktop/src-tauri/tauri.conf.json`. Its private key stays on the primary
release Mac at `~/.tauri/dakia-updater.key` with mode `0600`; store its password
in the login Keychain:

```bash
./scripts/store-local-release-secret.sh updater-password
```

Keep an encrypted offline backup of both values. Losing them permanently breaks
updates for installed builds that trust this key.

The release Mac also needs a valid Developer ID Application certificate, a
working `dakia-notary` Keychain profile, the Google Desktop OAuth client secret,
and R2 credentials scoped to the `dakia-releases` bucket. Store the latter
without adding them to shell history:

```bash
./scripts/store-local-release-secret.sh google-oauth-client-secret
./scripts/store-local-release-secret.sh r2-access-key-id
./scripts/store-local-release-secret.sh r2-secret-access-key
```

Release scripts read these Keychain items only into their own process. The OAuth
secret is supplied only to the Dakia Rust compiler process that needs it.

## Artifacts and publication

The production updater endpoint is:

```text
https://downloads.dakiamail.com/macos/latest/latest.json
```

Local artifacts live under the ignored `release-assets/` directory:

```text
Dakia_<version>_aarch64.dmg
Dakia-aarch64.app.tar.gz
Dakia-aarch64.app.tar.gz.sig
release-notes.md
SHA256SUMS.txt
```

The updater archive is made from the final Developer ID signed, notarized, and
stapled app, including the CLI sidecar, ONNX Runtime, classifier resources, and
required notices.

## Release sequence

```bash
npm run verify:local
npm run release:build -- vX.Y.Z
npm run release:publish -- vX.Y.Z "$PWD/release-assets/vX.Y.Z"
```

`verify:local` runs the repository's Rust and frontend checks without producing
a redundant packaged app. `release:build` builds the Apple Silicon app, signs
and verifies it, notarizes and staples it, rebuilds and verifies the DMG, then
creates and Tauri-signs the final updater archive.

`release:publish` verifies the final DMG, refuses to overwrite versioned R2
objects, uploads the immutable DMG/archive/signature, and anonymously downloads
and byte-compares each. It generates and validates the Apple-Silicon-only Tauri
manifest, updates the stable DMG alias, and writes `latest.json` last. Therefore
a failed upload or public-byte check leaves installed clients on the previous
release.

There is no staging update feed or installed-client acceptance harness in this
early-development workflow. Unit tests cover the updater interface and manifest
structure; a real update/install/relaunch test should be restored before relying
on automatic updates for broad distribution.

Existing Intel installs are no longer supported and will not receive a matching
entry in future updater manifests. The public website links only to the Apple
Silicon download.

## Source-control marker

After R2 publication is proven, push a signed `vX.Y.Z` Git tag as a source
marker. It does not trigger a build. Create the GitHub Release from that
verified tag and copy the already-built local release assets to it; do not
rebuild assets on GitHub.
