# Publishing A macOS Release

Dakia releases are built, signed, notarized, tested, and published from the
primary Apple Silicon Mac. R2 is the only public artifact host. GitHub Actions
and GitHub Releases are intentionally not used.

## Preflight

Before release work:

```bash
git status --short
security find-identity -v -p codesigning
xcrun notarytool history --keychain-profile dakia-notary
rustup target list --installed
aws --version
```

Required state:

- the release tree is clean and already contains the intended code;
- one valid Developer ID Application identity is available;
- the `dakia-notary` profile can authenticate;
- `aarch64-apple-darwin` and `x86_64-apple-darwin` are installed;
- the updater private key and its Keychain password are available;
- the Google Desktop OAuth client secret is stored in Keychain;
- bucket-scoped R2 credentials are stored as described in
  [Signed Desktop Updates](updater-release.md).

Keep the version synchronized in `package.json`, the Cargo workspace and lock
file, and `apps/desktop/src-tauri/tauri.conf.json`.

## Commands

Run the local replacement for CI:

```bash
npm run verify:local
```

Build staging, perform the two-machine signed update acceptance suite, then
build and publish production:

```bash
npm run release:build -- vX.Y.Z staging
npm run release:publish:staging -- \
  vX.Y.Z "$PWD/release-assets/vX.Y.Z/staging"

# Follow the two-machine acceptance sequence in updater-release.md.

npm run release:build -- vX.Y.Z production
npm run release:publish:production -- \
  vX.Y.Z \
  "$PWD/release-assets/vX.Y.Z/production" \
  "$PWD/release-assets/vX.Y.Z/evidence"
```

The builder applies the required order independently to both architectures:

1. assemble and Developer ID sign the app;
2. statically verify the executable, classifier resources, third-party
   notices, and Dakia MPL 2.0/source-availability notice for **both**
   architectures;
3. run the packaged-app startup check when native to the machine;
4. notarize and staple the app;
5. rebuild the DMG from that final app;
6. notarize, staple, mount-verify, and repeat static verification of the DMG;
7. archive that same final app and sign the exact archive bytes.

The Intel app receives its actual startup and updater/restart proof on the
Intel MacBook, not under Rosetta. An Intel native-execution waiver never skips
the architecture-independent app/resource/legal verification.

## Final audit

After production promotion:

```bash
curl -I https://downloads.dakiamail.com/macos/vX.Y.Z/Dakia-Apple-Silicon.dmg
curl -I https://downloads.dakiamail.com/macos/vX.Y.Z/Dakia-Intel.dmg
curl -I https://downloads.dakiamail.com/macos/latest/Dakia-Apple-Silicon.dmg
curl -I https://downloads.dakiamail.com/macos/latest/Dakia-Intel.dmg
curl -fsS https://downloads.dakiamail.com/macos/latest/latest.json |
  node scripts/updater-manifest.mjs validate --manifest /dev/stdin
```

Also retain the ignored local artifact directory, its `SHA256SUMS.txt`, and all
six acceptance entries in the encrypted release archive. An explicit waiver is
an acceptance entry, not a passing test; keep it with the executed evidence and
complete the deferred verification later.

See [Signed Desktop Updates](updater-release.md) for the full staging fixture,
Intel handoff, evidence, and atomic publication procedure.
