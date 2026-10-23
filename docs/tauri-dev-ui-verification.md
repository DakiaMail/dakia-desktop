# Verifying the Intended Tauri Dev App on macOS

This guide exists because macOS can have several applications named **Dakia**
registered at the same time. A UI automation tool asked to control `Dakia` by
display name may launch an installed, bundled, or mounted-DMG copy instead of
attaching to the raw executable started by `npm run dev`.

The consequence is serious: a screenshot can look plausible while proving
nothing about the source currently under development.

## The authoritative dev runtime

For native/backend behavior, start the app with:

```sh
npm run dev
```

This runs `scripts/dev.sh`, which in turn runs Tauri dev and starts Vite on
port 1420. `npm run dev:web` is only the browser demo and is not evidence for
Tauri IPC, plugins, native windows, Keychain behavior, or external URL opening.

Before interacting with a window, verify both sides of the dev runtime:

```sh
lsof -nP -iTCP:1420 -sTCP:LISTEN
ps -axo pid,lstart,command | rg \
  'target/debug/dakia-desktop|Dakia.app/Contents/MacOS|/Volumes/Dakia'
```

Expected evidence for the normal dev process:

- Vite is listening on `127.0.0.1:1420`.
- The native process path ends in the current checkout's
  `target/debug/dakia-desktop`.
- The desktop webview is at `127.0.0.1:1420/`.

Treat `tauri://localhost` as bundled-content evidence, not live Vite dev
evidence.

## Builds that are easy to confuse

The following are distinct from the raw `npm run dev` process and may contain
older frontend assets:

- `/Applications/Dakia.app`
- `target/debug/bundle/macos/Dakia.app`
- `target/release/bundle/macos/Dakia.app`
- `Dakia.app` inside a mounted DMG under `/Volumes`
- a binary launched with plain `cargo run` rather than Tauri dev

Do not use one of these to verify a dev-server change unless the task is
specifically about that packaged artifact.

## Why controlling `Dakia` by name is unsafe

The raw Tauri dev executable is not necessarily registered as an addressable
macOS application. Meanwhile, Launch Services may know about several bundles
whose display name is `Dakia` and whose bundle identifiers overlap.

Therefore:

- Do not call UI automation with only the display name `Dakia` when validating
  `npm run dev`.
- Do not assume that bringing a window named `Dakia` forward attached to the
  already-running dev process.
- After every automated launch or attachment, re-check the running process
  path. If a bundle path appears, discard that UI evidence.
- Check the webview URL in the accessibility tree. For this repository's live
  dev desktop route it must be `127.0.0.1:1420/`.

## Unambiguous Computer Use verification

For repeated fictional-mailbox checks, reuse the previously approved test
application name, bundle identifier, and application path. Use the harness’s
sealed approved-identity record and guarded replacement/recovery path. Each run
still needs fresh executable hashes, process identity, source provenance, and
webview URL verification. Reusing a display name alone is not identity proof.
Never overwrite an installed user app to reuse its identity.

The fictional provider harness can also build an isolated ad-hoc packaged
candidate for bundled-content acceptance. Its live URL is `tauri://localhost`,
which must be reported as packaged evidence. It does not establish live Vite
dev behavior, Developer ID signing, notarization, or public release acceptance.

When the UI tool cannot target the raw dev executable, use a temporary,
uniquely identified wrapper around a binary compiled from the current checkout.
The wrapper is a test harness, not a release artifact.

1. Keep the real Vite dev server on port 1420 running.
2. Create a temporary Tauri config that:
   - points `build.devUrl` to `http://127.0.0.1:1420/`;
   - disables `beforeDevCommand`, because Vite is already running;
   - gives the verification window a unique title;
   - keeps the compiled Dakia identifier when existing local mail data is
     needed;
   - disables bundling.
3. Compile with an isolated `CARGO_TARGET_DIR` under `/private/tmp` so the
   repository's ordinary dev binary is not replaced.
4. Wrap that exact temporary executable in a temporary `.app` with a unique
   `CFBundleDisplayName` and `CFBundleIdentifier`, then ad-hoc sign the wrapper.
5. Launch the wrapper and target its unique bundle identifier, never `Dakia`.
6. Confirm all three facts before clicking:
   - the process path is the temporary wrapper created for this test;
   - no installed or bundled Dakia process was launched;
   - the accessibility tree reports `127.0.0.1:1420/`.
7. Stop only the temporary processes and remove only the explicitly verified
   temporary paths after testing. Leave the user's normal `npm run dev`
   process alone.

Never claim runtime verification if any of those identity checks is ambiguous.

## Verification evidence should match the behavior

Static checks and browser tests are useful but cannot prove native behavior.
For a native action, collect evidence from both sides of the boundary.

For example, an email-link test should prove that:

1. The tested window is the current Tauri dev app at
   `127.0.0.1:1420/`.
2. A real HTML email containing links is open.
3. A normal link click leaves the Dakia webview at `127.0.0.1:1420/`.
4. The operating system's registered default browser opens the expected URL.
5. The browser's window or tab tree shows the new destination.

Checking only that the Dakia webview did not navigate is incomplete. Checking
only that a browser tab opened is also incomplete.

## Other lessons from the email-link investigation

- HTML email renders in an opaque sandboxed iframe without `allow-same-origin`.
  Its trusted resize bridge caps the outer height at 6000 pixels; longer content
  must remain reachable through internal scrolling. Verify both growth and
  shrinkage in WebKit, including repeated history disclosure cycles. Continue to remove
  email-authored scripts, inline `on*` handlers, frames, forms, `srcdoc`, and
  SVG mutation elements before mounting it.
- Remove every sanitized anchor's navigable `href`, store the validated
  destination in a Dakia data attribute, and validate it again when activating
  the link. This keeps a missed or mutated handler from navigating the app
  webview.
- When changing the renderer boundary, test hostile sender CSS and dynamic SVG
  URL mutation as well as static active-content attributes.
- Browser-only Playwright verification cannot establish that Tauri's opener
  plugin reached the macOS default browser.

## Reporting standard

State exactly what was verified and what was not. A good handoff includes:

- launch command;
- native executable path;
- webview URL;
- concrete interaction performed;
- observed native result;
- relevant static checks;
- any temporary test processes or artifacts removed.

If the window identity was not proven, say that verification is blocked rather
than presenting screenshots from an uncertain build.

## Fictional attachment filesystem acceptance

`scripts/native-attachment-fault-acceptance.sh` exercises real macOS filesystem
failures against the owned profile created by
`scripts/native-provider-reader-acceptance.sh`. Keep fixture artifacts, prove the
exact executable and live URL, and leave the app running until Downloads has
been restored. The helper rejects unrelated roots, symlinks, stale evidence,
changed mount identities, and oversized images.

Before saving `meeting-notes.txt` or using Save all, run the leaf collision case:

```sh
./scripts/native-attachment-fault-acceptance.sh leaf-prepare --root "$RUN_ROOT"
# Click Save quarterly-report.pdf.command in Reader once; wait for completion.
./scripts/native-attachment-fault-acceptance.sh leaf-check --root "$RUN_ROOT"
```

The check requires the symlink and its canary to remain unchanged, exact saved
bytes, a collision filename, mode 0600, and macOS quarantine. Record the visible
warning, completion feedback, and absence of automatic opening separately.

With the UI idle, prepare and fill the bounded volume:

```sh
./scripts/native-attachment-fault-acceptance.sh enospc-prepare --root "$RUN_ROOT" --ui-idle
./scripts/native-attachment-fault-acceptance.sh enospc-fill --root "$RUN_ROOT"
```

This creates and validates an exact 64 MiB read-write HFS+ image before moving
the original fictional Downloads directory. It mounts only at that owned path.
The bounded fill requires actual ENOSPC and verifies the remaining free space.
If an interrupted large-block fill left the authenticated filler and ENOSPC log,
`enospc-fill-resume` validates their identity before bounded tail appends. Do not
repeat image preparation or bypass a failed identity check.

After a passing fill, click Save meeting-notes.txt once. Verify a visible error
and cleared spinner, then check the filesystem and restore it:

```sh
./scripts/native-attachment-fault-acceptance.sh enospc-check --root "$RUN_ROOT"
./scripts/native-attachment-fault-acceptance.sh enospc-restore --root "$RUN_ROOT" --ui-idle
# Retry Save meeting-notes.txt once; wait for visible success.
./scripts/native-attachment-fault-acceptance.sh enospc-retry-check --root "$RUN_ROOT"
```

The failure check rejects final or partial attachment files. Restoration checks
the exact image, device, and original directory identity. The retry check requires
exact bytes, safe permissions, quarantine, and no partial files. Detach and
restore before quitting the app or ending its outer fixture harness.

Stop on any unexpected failure and retain the root and evidence. An incomplete
run is not a pass. Inspect the recorded image and mount identity before cleanup;
do not force-detach a guessed device or remove a mounted fixture root.

`native-attachment-fault-acceptance.node-test.mjs` runs extracted filesystem
oracles and mutation-sensitive rejection checks on Linux and macOS. The full
synthetic shell suite runs only on macOS. Neither source suite creates or mounts
a disk image. Actual image creation, native save failure, and recovery remain
separate manual evidence.

## Preparing sealed fictional profile copies

Use `scripts/native-profile-fixture.mjs` for the filesystem preparation step of
schema and recovery checks. It accepts an owned sealed source directory with
mode 0500 or 0700 and a private mode-0600 manifest. The manifest declares exact
filenames, byte counts, numeric modes and SHA-256 values, explicit owned markers,
and files that must be absent. The contract is `dakia-native-profile-fixture-v1`,
with `schemaVersion`, `files`, `ownedMarkers` and `expectedAbsentFiles` fields.
Each file entry has `path`, `bytes`, `mode` and `sha256` fields. It must include
`dakia.db` and `vault.key`; it cannot contain arbitrary relative paths.

```sh
node scripts/native-profile-fixture.mjs prepare \
  --source SEALED_FICTIONAL_PROFILE \
  --manifest PRIVATE_MANIFEST_JSON \
  --output-root NEW_PRIVATE_DIRECTORY_UNDER_OS_TEMP
```

The new output contains `profile/` and a preparation receipt. Directories use
0700 and copied files use 0600, including copies of read-only backup files. The
source remains unchanged. Existing output paths are rejected. Retain the complete
source and output; this helper never installs a profile in Application Support,
launches an app or changes the approved application identity.

After the candidate exits, `compare --baseline PRIVATE_MANIFEST_JSON --after
PROFILE_COPY --policy future-profile-durable` verifies the named database, key and
owned markers while requiring the WAL to remain absent. The generic policy
reports an inventory without an acceptance verdict. Neither comparison proves a
schema migration, complete-profile quiescence, process exit or native UI result.
Those require the separate production Store and native observations above.
