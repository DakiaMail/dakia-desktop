# Native mail safety acceptance

This record covers the macOS WebKit boundary for remote email content and the
unsafe-attachment warning. Source tests remain the primary evidence. This lane
runs only after the unit, integration, type, build, and static gates pass.

The fixture mailbox is fictional and is enabled only by the Vite development
flag `VITE_DAKIA_DEMO_API=1`. The native backend must also use a temporary
Tauri identifier. The demo flag changes frontend data only; by itself it does
not isolate the Rust backend from the normal Dakia profile.

## Required launch identity

Start the loopback request counter:

```sh
node scripts/native-mail-safety-fixture-server.mjs \
  --port 48731 \
  --evidence /private/tmp/dakia-native-mail-safety-requests.json
```

Create a temporary Tauri override with a unique identifier, no
`beforeDevCommand`, and the existing loopback Vite URL. For example:

```json
{
  "productName": "Dakia Mail Safety Acceptance",
  "identifier": "dev.dakia.mail-safety-acceptance",
  "build": {
    "beforeDevCommand": "",
    "devUrl": "http://127.0.0.1:1420/"
  },
  "bundle": { "active": false }
}
```

Start Vite and Tauri from this checkout, with a dedicated Cargo target when
the normal target is in use by another test lane:

```sh
VITE_DAKIA_DEMO_API=1 \
VITE_DAKIA_MAIL_SAFETY_ORIGIN=http://127.0.0.1:48731 \
npm run dev:web

CARGO_TARGET_DIR=/private/tmp/dakia-native-mail-safety/target \
npx tauri dev \
  --config apps/desktop/src-tauri/tauri.conf.json \
  --config /private/tmp/dakia-native-mail-safety/tauri.json
```

Before opening any fixture message, query the database under
`~/Library/Application Support/<temporary-identifier>/dakia.db`. It must
exist and contain zero accounts. Stop and discard the run if the backend opens
the normal `dev.dakia.mail` profile.

Before interacting, record both commands below and inspect the native
accessibility tree:

```sh
lsof -nP -iTCP:1420 -sTCP:LISTEN
ps -axo pid,lstart,command | rg \
  'target/debug/dakia-desktop|Dakia.app/Contents/MacOS|/Volumes/Dakia'
```

Accepted identity requires all of these facts:

- Vite listens on `127.0.0.1:1420`.
- The native process is this checkout's `target/debug/dakia-desktop`, or the
  exact temporary wrapper compiled from this checkout according to
  [the Tauri dev verification guide](tauri-dev-ui-verification.md).
- The native accessibility tree reports `127.0.0.1:1420/` for the tested
  webview.
- No installed, bundled, or mounted-DMG Dakia process supplies the tested
  window.

Evidence from a window that fails any identity check must be discarded.

## Acceptance sequence

1. Reset the counter with `curl -fsS -X POST http://127.0.0.1:48731/reset`.
2. Open the fictional message `Remote content safety check`.
3. Read `http://127.0.0.1:48731/requests`. It must report zero requests. The
   local embedded image and the `Load remote content` button must be visible.
4. Click `Load remote content` once, then read `/requests` again. Only
   `/remote-image.svg`, loaded from an HTTP(S) `<img src>`, may be requested.
   Sender stylesheets, CSS images, fonts, `srcset`, media, SVG subresources,
   scripts, frames, forms, and execution canaries must remain at zero.
5. Collapse and expand the same message twice. Its text, embedded image,
   disclosure control, and layout must remain usable with no app navigation or
   executed hostile content.
6. Open a different fictional conversation and return. The original message
   must show `Load remote content` again. Reading `/requests` before granting
   permission again must show no new remote request caused by the return.
7. Confirm the attachment `quarterly-report.pdf.command` shows Dakia's unsafe
   attachment warning. Do not treat the demo save result as filesystem
   evidence because the demo API does not write a file.

The only permitted resource route after visible, per-message consent:

- `http://127.0.0.1:48731/remote-image.svg`

Routes that must remain unrequested:

- `http://127.0.0.1:48731/srcset-image.svg`
- `http://127.0.0.1:48731/svg-external.svg`
- `http://127.0.0.1:48731/remote-style.css`
- `http://127.0.0.1:48731/background.svg`
- `http://127.0.0.1:48731/remote-font.woff2`
- `http://127.0.0.1:48731/hostile-script.js`
- `http://127.0.0.1:48731/hostile-frame.html`
- `http://127.0.0.1:48731/form-submit`
- `http://127.0.0.1:48731/script-executed`
- `http://127.0.0.1:48731/frame-executed`

## Evidence record

Attempted on 2026-10-17 from source revision
`fccbace81192c5b5354f681e2feb2b66971e88cf`. Source changes under test were
uncommitted, so this revision alone is not an immutable description of the
renderer. The focused source-test evidence belongs with the final parent test
report.

The isolated native process started, but the accessibility automation could
not attach to it. Both the exact bundle identifier and exact app path forms of
`cua.getApp(...)` remained blocked until externally interrupted. No visible UI
state or click was observed. All UI-dependent gates below therefore remain
unavailable rather than passed.

An earlier launch attempt used `VITE_DAKIA_DEMO_API=1` with the normal
`dev.dakia.mail` backend identifier. Although the frontend data was fictional,
the Rust process opened the normal local profile and logged background realtime
work for an existing account. The process was stopped as soon as this was
observed, before any UI interaction, send, settings change, or deliberate mail
mutation. That attempt is discarded. It may have performed background provider
reads or local state work, so it is not evidence of profile isolation or zero
external access. This finding is why the procedure now requires a unique Tauri
identifier and a zero-account database check before interaction.

| Gate                                                  | Status  | Evidence                                                                                                                                                                                                                                                                                                                                         |
| ----------------------------------------------------- | ------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Fixture server unit tests                             | Passed  | `node --test scripts/native-mail-safety-fixture-server.node-test.mjs`, 3 tests passed on 2026-10-17                                                                                                                                                                                                                                              |
| Focused renderer and Reader tests                     | Passed  | Final renderer policy run: `HtmlMessage.test.tsx` passed 29/29 and the Reader identity-reset target passed 1/1. A sensitivity perturbation that enabled remote content by default made the blocking regression fail, then the restored implementation passed. The parent report owns the exact final command transcript                          |
| Source, integration, build, and static gates          | Skipped | Parent verification was still receiving unrelated content-epoch changes; do not infer a full current source pass from this lane                                                                                                                                                                                                                  |
| Exact native executable path                          | Passed  | PID 81813 ran `/private/tmp/dakia-native-mail-safety-20261017/Dakia Mail Safety Acceptance.app/Contents/MacOS/dakia-desktop`; `ps -axo pid,lstart,command` captured the exact path                                                                                                                                                               |
| Isolated backend profile                              | Passed  | Bundle test identifier `dev.dakia.mail-safety-acceptance`; `sqlite3 "$HOME/Library/Application Support/dev.dakia.mail-safety-acceptance/dakia.db" 'select count(*) from accounts;'` returned `0` before and after the attempt                                                                                                                    |
| Loopback frontend identity                            | Passed  | PID 70819 was `node .../node_modules/.bin/vite --config apps/desktop/vite.config.ts`; `lsof -nP -iTCP:1420 -sTCP:LISTEN` showed only `127.0.0.1:1420`                                                                                                                                                                                            |
| Executable architecture and integrity                 | Passed  | `file` reported Mach-O 64-bit arm64. Pre-wrapper debug executable SHA-256 was `e75c7833799b9aad786b985b985bfbaee37dadc668d88e2385fec7fb119b9c84`. The resource-complete, ad-hoc-signed wrapper executable SHA-256 was `148ea0fda756e0e50fd070ec7434a682bccac2d7e45ce03583205ced4472f94c`; `codesign --verify --deep --strict --verbose=2` passed |
| Live webview URL in accessibility                     | Failed  | Required native accessibility proof was unavailable because both exact-identity `cua.getApp(...)` calls hung until interrupted. The Vite listener alone does not prove which URL the webview displayed                                                                                                                                           |
| Default remote-content blocking                       | Skipped | The counter was reset and remained at zero for the idle isolated process, but the fictional message was never visibly opened. Idle zero is not evidence of default blocking                                                                                                                                                                      |
| Per-message consent                                   | Skipped | No visible `Load remote content` click occurred; no post-consent request evidence exists                                                                                                                                                                                                                                                         |
| Hostile code and frame isolation                      | Skipped | Final counter was zero, but the hostile message was not observed in the Reader, so this cannot establish isolation                                                                                                                                                                                                                               |
| Repeat collapse and expansion                         | Skipped | Native Reader interaction unavailable                                                                                                                                                                                                                                                                                                            |
| Permission reset after message switch                 | Skipped | Native Reader interaction unavailable                                                                                                                                                                                                                                                                                                            |
| Unsafe attachment warning                             | Skipped | Native Reader interaction unavailable                                                                                                                                                                                                                                                                                                            |
| Saved bytes, execute permission, and macOS quarantine | Skipped | Demo API deliberately does not write files; this needs a production-path isolated mailbox fixture                                                                                                                                                                                                                                                |
| Apple Silicon process launch                          | Passed  | Host `uname -m` returned `arm64`; `file` confirmed the launched executable was arm64. This is process evidence only, not UI acceptance                                                                                                                                                                                                           |
| Intel native run                                      | Waived  | Owner-authorized architecture waiver; never counted as a pass                                                                                                                                                                                                                                                                                    |
| Test process cleanup                                  | Passed  | Exact PIDs 81813 (wrapper), 70819 (Vite), and 20304 (fixture server) were stopped. The temporary build, wrapper, and zero-account profile were deleted after evidence capture. `/private/tmp/dakia-native-mail-safety-requests.json` preserves the final zero-request record                                                                     |

## Remaining release risk

The 2026-10-17 attempt did not establish actual WebKit loading behavior because
the native accessibility controller never attached. It did establish that the
test process used an arm64 executable, a unique test identifier, a zero-account
profile, and loopback-only fixture services. The final zero-request record
shows that the idle harness made no contact; it does not prove behavior after a
message is rendered or consent is clicked.

A later native run must repeat the full sequence against the final renderer and
capture the webview URL, visible Reader state, pre-consent counter, post-consent
`/remote-image.svg` request, forbidden-route zeros, repeated disclosure, and
identity reset. Production IMAP parsing, real attachment bytes, filesystem
permissions, quarantine metadata, packaged-app behavior, updater behavior,
Apple signing, notarization, and stapling remain separate integration and
release gates. The wrapper used only a local ad-hoc signature for test launch;
it was not a Developer ID or updater signature. No release action was taken.

## Final CLI-native retry

A second isolated attempt started after three consecutive source baselines and
the deeper server and fuzz lanes passed against frozen source manifest SHA-256
`b4433ab96dca86ca6861d64d7d39a6489e5d30daabb022ddc563bba7d1751013`.
This retry uses command-line macOS Accessibility and exact-window capture tools
instead of CUA.

The temporary build and harness were prepared outside the repository:

- App identifier: `dev.dakia.mail-safety-final-acceptance`
- Wrapper identifier: `dev.dakia.mail-safety-final-acceptance.wrapper`
- Exact executable: `/private/tmp/dakia-native-mail-safety-final-20261017/Dakia Mail Safety Final Acceptance.app/Contents/MacOS/dakia-desktop`
- Debug executable SHA-256: `199a76fd8f42bbcd56b65619299e31c94e1b8e307019e080578ff3aa2d003492`
- Ad-hoc-signed wrapper executable SHA-256: `38af6c0e40cc86c5398d4fb93b28c66661a5e6a4ac545854d3ee2732eb042e1f`
- Temporary Tauri config SHA-256: `e6bb8f5108630763f0445bf5ce414c4dbb3ccb449087d13918c9edfbf2e9af54`
- Temporary event harness SHA-256: `2fec9f80dcb18e82702fe87bc486f102bc37f378b192626a5d151f9d1ee77e2e`
- Temporary Vite config SHA-256: `0e1059851848c7deb235b9d3589cd9060aabcdd92b87368718b96ef65b776841`

Before UI work, PID 47002 matched the exact executable above, `file` reported
an arm64 Mach-O executable, the dedicated database contained zero accounts,
Vite listened only on `127.0.0.1:1420`, and the reset fixture counter reported
zero requests. CoreGraphics reported one on-screen layer-zero app window,
window 5876, owned by PID 47002.

The UI sequence is paused because the macOS session is locked.
`CGSessionCopyCurrentDictionary` reported `CGSSessionScreenIsLocked=1` and a
completed login session. In this state, the exact app's Accessibility window
relationship resolves incorrectly, CUA cannot attach, and both legacy and
ScreenCaptureKit exact-window captures fail. No Reader control was pressed and
no screenshot was accepted as evidence. Dakia, Vite, and the fixture counter
remain running with the zero-account test profile while explicit user unlock is
pending. The retry must re-prove PID, profile, WebView URL, and a zero request
counter after unlock before any interaction.

The optional realtime check uses a visible temporary harness outside the React
root. It may call only `api.setStarred("demo-mail-safety", value)` to prepare
fictional demo state, followed by Tauri
`emit("mail-changed", { accountId })`. It does not call `loadMessages`, mutate
React state, dispatch a DOM mail event, or replace an event adapter. If the UI
changes only after the native emit, the evidence covers the real Tauri
AppHandle-to-Wry-to-production-listener boundary. It does not prove a provider
source or database sink; those remain separate source-test evidence.

The first binary prepared for this retry was discarded after later test-only
changes altered the overall source manifest. The unique wrapper was rebuilt
after the final writer freeze. The repository's `sourceSnapshot()` returned
the same value immediately before and after this build:

- Algorithm: `sha256-path-and-content-manifest`
- Digest: `f1496779d09eadec537a4830f8de0dd4f33d93835c007d779273fd8b2f9ebb4a`
- Files: 259
- Final debug executable SHA-256: `713449a40dd82434f681a767c6067e294d5eec8c4d3dc9a3833da0f05afe8312`
- Final ad-hoc-signed wrapper executable SHA-256: `ec37e7ad6194033fe5641c91caa069c92ae0ad87964526aef7b52fba14b54c2b`

`codesign --verify --deep --strict --verbose=2` passed for the rebuilt
wrapper, `file` reported arm64 for both executables, and the preserved isolated
database still contained zero accounts. The rebuilt wrapper remains stopped
until the user confirms that the macOS session is unlocked. Its source baseline
result must be taken from the final parent report; a successful build does not
replace that test evidence.

## Final post-fix native acceptance

The earlier `f149...` wrapper and a later `d4a52...` wrapper were both
discarded before UI acceptance. The deeper fuzz lane found, respectively, an
upstream parser panic and a cyclic MIME part graph that could cause a very large
allocation. The final native run happened only after both fixes, their retained
regressions, debug and release parser tests, and the post-fix fuzz campaign had
passed.

The accepted source snapshot was stable before and after the full source run
and the native build:

- Source algorithm: `sha256-path-and-content-manifest`
- Source digest: `ad502e348cd774298b68d7a00f0816cc8914b41a592acca0f54641d23abf6130`
- Source files: 664
- Source revision: `fccbace81192c5b5354f681e2feb2b66971e88cf` plus the recorded modified tree
- First full source report: `/private/tmp/dakia-native-mail-safety-final-20261017/final-source-one-host-retry.json`
- First full source report SHA-256: `33faeeb6d182e42e5c644ab444a3aa70b8b9080f7290fc52b0f3862b4af92324`
- Second and third full source report: `/private/tmp/dakia-native-mail-safety-final-20261017/final-source-repeat2-host.json`
- Second and third full source report SHA-256: `ab49d4ee09d5997d60538105e06c55df7892ad80bf8e70f7366263ee0ada63cc`

One earlier host source run on the same `ad502e...` source digest failed in
`ComposeApp send lifecycle > restores an uncertain draft and attachment after
reload without another submission`. The restored subject was present, but the
assertion at `ComposeApp.test.tsx:422` expected `Reload body` and received empty
text. No source or declared environment change occurred before the passing
runs. Fresh Vitest processes then passed the focused Compose suite six times,
one related six-file group, one full frontend suite, and the three accepted
source iterations. This is an unresolved same-code flake or restore timing
defect. The three passing iterations are factual, but they do not establish a
fully deterministic baseline or a proven root-cause fix.

The exact native identity was:

- Temporary app identifier: `dev.dakia.mail-safety-final-acceptance`
- Wrapper identifier: `dev.dakia.mail-safety-final-acceptance.wrapper`
- Executable: `/private/tmp/dakia-native-mail-safety-final-20261017/Dakia Mail Safety Final Acceptance.app/Contents/MacOS/dakia-desktop`
- Native PID: 34823, started Saturday 17 October 2026 at 18:36:22
- Native window: CoreGraphics window 6052, owned by PID 34823
- WebKit accessibility URL: `http://127.0.0.1:1420/`
- Debug executable SHA-256: `e97a260b3285390b02fdf42e1e680dfbffdd20568bd90f490f6d5ec970a6bd15`
- Ad-hoc-signed wrapper executable SHA-256: `9fd8c432d1bc78b89229689f391641416c0b5d9c771be21b8fd9e544b444100a`
- Architecture: arm64
- Strict ad-hoc code-sign verification: passed
- Isolated database accounts before launch: 0
- Analytics choice: kept off in the isolated profile

The temporary harness inputs were outside the repository and are retained for
review in `/private/tmp/dakia-native-mail-safety-final-20261017/`:

- Tauri override SHA-256: `e6bb8f5108630763f0445bf5ce414c4dbb3ccb449087d13918c9edfbf2e9af54`
- Vite harness module SHA-256: `2fec9f80dcb18e82702fe87bc486f102bc37f378b192626a5d151f9d1ee77e2e`
- Vite config SHA-256: `0e1059851848c7deb235b9d3589cd9060aabcdd92b87368718b96ef65b776841`
- Exact-PID AX helper source SHA-256: `b7fe744ec7f7506163c5b4a48b6b6d470fe7011997931680aedd00fb02b58cbd`

The final request evidence is
`/private/tmp/dakia-native-mail-safety-final-20261017/final-request-evidence.json`,
SHA-256 `f265535ffae7382c51a218c623e572f4b8376d99e6fd6e692d19688dfcddd346`.
It contains exactly one request, `GET /remote-image.svg`, after the visible
consent action. Every forbidden route listed above has a zero count. There was
no request before consent, no new request during two expand-collapse cycles,
and no new request after switching messages and returning before granting
consent again.

| Gate                                                               | Status                  | Final evidence                                                                                                                                                                                                                                                                                                                                                                                                      |
| ------------------------------------------------------------------ | ----------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Source, integration, build, and static gates                       | Passed with known flake | Three consecutive source iterations passed. The first report used `--repeat 1`; the second report used `--repeat 2`. Both reports recorded the same `ad502e...` snapshot before and after every run. An earlier full frontend run on that same digest observed an empty restored draft body. Fresh-process retries passed, but no root cause or corrective change was established.                                  |
| Exact executable and WebKit URL                                    | Passed                  | `lsof` identified PID 34823's exact temporary executable. AX exposed one `AXWebArea` with URL `http://127.0.0.1:1420/`. CoreGraphics window 6052 belonged to PID 34823.                                                                                                                                                                                                                                             |
| Isolated fictional profile                                         | Passed                  | The unique app identifier opened its own SQLite database. `select count(*) from accounts` returned 0 before launch. No real provider credentials or mailbox writes were used.                                                                                                                                                                                                                                       |
| Default remote-content blocking                                    | Passed                  | The fictional safety message and `Load remote content` control were visible while `/requests` reported zero. The local marker remained present.                                                                                                                                                                                                                                                                     |
| Per-message image consent                                          | Passed                  | AX pressed the visible `Load remote content` button. WebKit requested only `/remote-image.svg` once, then visibly rendered “Remote image loaded after consent.”                                                                                                                                                                                                                                                     |
| Styles, fonts, media, SVG subresources, scripts, frames, and forms | Passed                  | `/srcset-image.svg`, `/svg-external.svg`, `/remote-style.css`, `/background.svg`, `/remote-font.woff2`, `/hostile-script.js`, `/hostile-frame.html`, `/form-submit`, `/script-executed`, and `/frame-executed` all stayed at zero before and after consent.                                                                                                                                                         |
| Application control containment                                    | Passed                  | Exact-window screenshots show the message content remained inside its surface. Compose, mail actions, folder controls, and the trusted harness remained visible and usable after consent.                                                                                                                                                                                                                           |
| Repeated disclosure layout                                         | Passed                  | A two-message fictional thread completed collapse, expand, collapse, expand, and collapse through AX controls. Both expanded captures were byte-identical and no extra request occurred.                                                                                                                                                                                                                            |
| Permission reset after message switch                              | Passed                  | After opening another conversation and returning, `Load remote content` was present again and the request total remained one.                                                                                                                                                                                                                                                                                       |
| Unsafe attachment warning                                          | Passed                  | Reader exposed `quarterly-report.pdf.command`, `application/x-sh`, and “This file may run software. Only open it if you trust the sender.”                                                                                                                                                                                                                                                                          |
| Native event to production listener                                | Passed with scope limit | Preparing demo starred state through `api.setStarred` did not change the Reader. The real Tauri SDK `emit("mail-changed", { accountId })` changed `Star conversation` to `Remove star`. The inverse preparation also stayed invisible until the inverse native emit changed it back. This proves AppHandle to Wry to production-listener delivery only. It does not prove a provider event source or database sink. |
| Attachment bytes, execute bits, and quarantine                     | Skipped                 | The demo API simulates saving and does not write a file. Production-path Rust tests cover bytes and metadata; macOS quarantine still needs a production-path native fixture.                                                                                                                                                                                                                                        |
| Packaged release, Developer ID, notarization, stapling, updater    | Skipped                 | This was a development WebKit acceptance wrapper with an ad-hoc signature. No release action was authorized or performed.                                                                                                                                                                                                                                                                                           |
| Intel native run                                                   | Waived                  | The owner-authorized Intel architecture waiver remains separate and is not counted as a pass.                                                                                                                                                                                                                                                                                                                       |
| Test process cleanup                                               | Passed                  | Exact native PID 34823 was stopped after evidence capture. Loopback services are stopped after their final evidence file is flushed.                                                                                                                                                                                                                                                                                |

Exact-window screenshots retained in the temporary evidence directory:

- `remote-before-consent.png`, SHA-256 `2d4d88eaf91101861c4db2d0a3d2880196dfb322509a38dfac994c472ce2791d`
- `remote-after-consent.png`, SHA-256 `66b44bc4b9a0f4afee207489217a63b6cb10ef2b96314f00813a22f89687f89d`
- `remote-reset-after-switch.png`, SHA-256 `7f8628ab99d899ae5406af49e910227b74deda13888877bf9687402f5e7fb42d`
- `thread-both-collapsed.png`, SHA-256 `0bdf9446499ddb2eb685afbdfd3605f12ca1af19d22aa4b1e6572afb4f1c7b26`
- `thread-expanded-1.png` and `thread-expanded-2.png`, identical SHA-256 `48921597136e6ac778785d591ff8dd6a620a817106d4a7f480ca745326f5750c`
- `k015-prepared-starred.png`, SHA-256 `21acdeb50dd8de20441c7ff6996c08d40296ff7e4ea12e0b1d08baa9d4f183d1`
- `k015-emitted-starred.png`, SHA-256 `6d8dc463fc0f7aa5ae12a945c93c332974baa254f93c5d74d249ae86b39442ac`
- `k015-prepared-unstarred.png`, SHA-256 `8383121f387649aee2ba7c9927521a7b7e1c7fa9b1fa61f100f2e3952db731d4`
- `k015-emitted-unstarred.png`, SHA-256 `b4ac9029a3e05739a234f5ae3d315b4de2bcad49ab9454054fdb5061a3b6ea51`

The final native run proves the macOS WebKit privacy behavior of the fictional
development fixture and the scoped native event listener. It does not turn the
demo mailbox into provider evidence. Live providers, packaged upgrades,
production attachment saving and quarantine, Developer ID signing,
notarization, stapling, and Tauri updater signing remain distinct release
evidence.

## Subsequent test-readiness correction

The earlier same-source compose failure is retained, rather than erased by its
three later passes. Its assertion checked the editor before RichTextEditor’s
passive initialization effect had populated the restored HTML. The test now
waits for the body content itself. A deliberate empty-editor mutation still
fails that assertion, and the restored implementation passes.

Three fresh complete source iterations then passed on manifest
`6777974a12f401ab5d8fbb727d4fdf3c993e9ebeb1839848571ea44ab2066ea0`.
The only source difference from the native run is that test-only assertion.
The [build-input equivalence record](mail-safety-results/native-build-input-equivalence-2026-10-17.json)
proves all other 663 source inputs are identical. The product code tested
natively is unchanged. The durable native screenshots, request counter, and
harness inputs are indexed in
[the evidence index](mail-safety-results/native-2026-10-17/evidence-index.json).
The fresh source report is
[source-readiness-fixed-2026-10-17.json](mail-safety-results/source-readiness-fixed-2026-10-17.json).
