# Signed Desktop Updates

Dakia uses Tauri's native updater. Tauri downloads over the native transport,
verifies the archive with the public key embedded in the app, and installs only
after the user chooses **Install and Restart**. GitHub Actions and GitHub
Releases are not part of the release or update trust path.

## Local trust material

The permanent updater public key is embedded in
`apps/desktop/src-tauri/tauri.conf.json`. Its private key stays on the primary
release Mac at `~/.tauri/dakia-updater.key` with mode `0600`. Store its password
in the login Keychain:

```bash
./scripts/store-local-release-secret.sh updater-password
```

Keep an encrypted offline backup of both values. Losing them permanently breaks
updates for installed builds that trust this key.

The primary Mac also needs:

- a valid **Developer ID Application** certificate and private key in Keychain;
- a working `notarytool` Keychain profile named `dakia-notary`;
- the Google Desktop OAuth client secret used for Gmail token exchange;
- an R2 S3 API token with **Object Read & Write** access limited to the
  `dakia-releases` bucket.

Store the Google OAuth and R2 values without adding them to shell history:

```bash
./scripts/store-local-release-secret.sh google-oauth-client-secret
./scripts/store-local-release-secret.sh r2-access-key-id
./scripts/store-local-release-secret.sh r2-secret-access-key
```

The local release scripts read these Keychain items into their own process and
never write their values into the repository. A release build now fails before
compilation if the Google OAuth secret is absent or Google rejects its pairing
with the configured desktop client ID. The preflight sends the secret to curl
through an owner-readable temporary file, never as a command-line argument.
Frontend and CLI preparation, dependency installation, the broader test suite,
Tauri, and Cargo run without the secret. A protected compiler wrapper injects
it only for the `dakia-desktop` Rust compiler process. The account ID is not
secret and is fixed in `scripts/local-release-env.sh`.

## Endpoints and artifacts

Production:

```text
https://downloads.dakiamail.com/macos/latest/latest.json
```

Staging:

```text
https://downloads.dakiamail.com/macos/staging/latest.json
```

Local artifacts live under the ignored `release-assets/` directory. For each
channel, the dual-architecture builder produces:

```text
Dakia_<version>_aarch64.dmg
Dakia_<version>_x64.dmg
Dakia-aarch64.app.tar.gz
Dakia-aarch64.app.tar.gz.sig
Dakia-x86_64.app.tar.gz
Dakia-x86_64.app.tar.gz.sig
release-notes.md
SHA256SUMS.txt
```

Each updater archive is created from the final Developer ID signed, notarized,
and stapled app containing the CLI sidecar, ONNX Runtime, and classifier
resources. Each architecture also carries and is statically checked for the
Dakia MPL 2.0/source-availability notice and bundled third-party notices,
even where native startup is waived.

## Local-first release sequence

Use an older staging build as the baseline. The examples below use `v0.2.11`
and `v0.2.12`.

### 1. Verify and build staging artifacts

```bash
npm run verify:local
npm run release:build -- v0.2.12 staging
npm run release:publish:staging -- \
  v0.2.12 "$PWD/release-assets/v0.2.12/staging"
```

The build runs sequentially on the primary Apple Silicon Mac. It compiles both
Rust targets, re-signs the fully assembled apps, submits and staples both apps,
rebuilds and notarizes both DMGs, then signs both updater archives.

### 2. Prepare the Intel MacBook

Create a portable kit from the exact older Intel DMG:

```bash
npm run release:test:intel-kit -- \
  v0.2.11 \
  "$PWD/release-assets/v0.2.11/baseline/Dakia_0.2.11_x64.dmg" \
  "$PWD/release-assets/v0.2.12/intel-test-kit"
```

Copy the resulting `.tar.gz` to the Intel MacBook, verify its adjacent SHA-256
file, extract it, and keep that directory for all three tests.

### 3. Prove all updater modes on both Macs

For each mode in this exact order:

```text
tampered-archive
invalid-signature
valid
```

First select the fixture and run the native Apple Silicon test on the primary
Mac:

```bash
npm run release:test:update -- \
  v0.2.11 v0.2.12 <mode> \
  "$PWD/release-assets/v0.2.11/baseline/Dakia_0.2.11_aarch64.dmg" \
  "$PWD/release-assets/v0.2.12/staging" \
  "$PWD/release-assets/v0.2.12/evidence"
```

Do not select the next fixture yet. While staging still points at that mode,
run this from the extracted kit on the Intel MacBook:

```bash
./run-test.sh v0.2.12 <mode>
```

Copy the kit's `evidence/x86_64` directory back into
`release-assets/v0.2.12/evidence/x86_64` on the primary Mac. Then select and test
the next mode.

The valid test proves check, download, verified install, relaunch into the new
version, and preservation of the isolated account/mail profile. The two
rejection tests prove a modified archive and an invalid embedded signature do
not install.

Verify the complete evidence set:

```bash
npm run release:test:verify -- \
  v0.2.12 "$PWD/release-assets/v0.2.12/evidence"
```

The normal gate requires six passing records: three fixture modes for
`aarch64` and three for `x86_64`.

If the release owner explicitly accepts deferred verification, a missing test
may instead have an auditable waiver:

```bash
./scripts/record-local-updater-waiver.sh \
  v0.2.12 x86_64 tampered-archive \
  "$PWD/release-assets/v0.2.12/evidence" \
  "Reason for deferring this exact test." \
  "Release owner name"
```

A waiver records the release, architecture, mode, reason, authorizer, and UTC
timestamp and is never reported as a pass. Waiving the `valid`
install-and-restart test additionally requires the release owner to acknowledge
the greater risk explicitly:

```bash
DAKIA_ALLOW_VALID_WAIVER=1 \
  ./scripts/record-local-updater-waiver.sh \
  v0.2.12 x86_64 valid \
  "$PWD/release-assets/v0.2.12/evidence" \
  "Production authorized with this upgrade test deferred." \
  "Release owner name"
```

Use waivers only under an explicit release-owner instruction. Perform the
deferred test later and start a corrective release if it exposes a problem.

### 4. Build and promote production

Production apps embed the production endpoint, so build them separately after
staging acceptance:

```bash
npm run release:build -- v0.2.12 production
npm run release:publish:production -- \
  v0.2.12 \
  "$PWD/release-assets/v0.2.12/production" \
  "$PWD/release-assets/v0.2.12/evidence"
```

The production publisher cannot run until all six architecture/mode entries
are accounted for by passing evidence or explicit waivers. It verifies both
final DMGs, refuses to overwrite versioned R2 objects, uploads immutable
artifacts for both architectures, downloads them anonymously and compares
their exact bytes, validates the Tauri manifest, then uploads `latest.json`
last. Any earlier failure leaves the production feed on its previous release.

## Source-control marker

After production publication is proven, a signed `vX.Y.Z` Git tag may be pushed
as a source marker. It does not trigger a build and it is not an artifact host.
Do not create a GitHub Release.
