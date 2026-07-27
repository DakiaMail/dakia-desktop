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
3. A normal link click leaves the email iframe at `about:srcdoc`.
4. The operating system's registered default browser opens the expected URL.
5. The browser's window or tab tree shows the new destination.

Checking only that the iframe did not navigate is incomplete. Checking only
that a browser tab opened is also incomplete.

## Other lessons from the email-link investigation

- A sandboxed iframe without `allow-scripts` suppresses trusted event callbacks
  installed by the parent in macOS WebKit. Dakia permits those callbacks while
  continuing to remove email-authored scripts, inline `on*` handlers, nested
  frames, forms, and `srcdoc`; the iframe CSP still uses `default-src 'none'`.
- Preserve a harmless fragment `href` for sanitized anchors and store the real
  URL separately. If interception fails, only the iframe fragment changes.
- Prefer a direct handler captured for each static email anchor over delegated
  cross-realm `composedPath()` logic.
- Construct `ResizeObserver` from the iframe's own `defaultView` when observing
  iframe elements in WebKit.
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
