# Receipt layout and dark-mode verification

Verified on 17 October 2026 using the production `HtmlMessage` component,
production `styles.css`, and the redacted Stripe receipt fixture. This is native
macOS WKWebView renderer acceptance. It does not test Tauri IPC, live mailbox
retrieval, signing, or an installed release.

## Problem and fix

Dakia added `overflow-wrap: anywhere` to table-based email. Stripe's description
cell requests 100% width, so WebKit could shrink the amount column to one
character. Dakia also removed intentional empty spacer rows. Finally, decorative
black header cells and unused background-image CSS disabled dark adaptation for
the entire light receipt.

The fix preserves authored tables and spacers, keeps aggressive wrapping on
ordinary reader content, and classifies current and quoted content separately.
It adapts light surfaces and muted blue-grey text while preserving authored dark
layouts, image-backed text, and light-backed artwork such as QR codes.

## Native results

| Check | Original renderer | Fixed renderer |
| --- | --- | --- |
| Summary amount cells | 11.171875 px wide, 192 px high | 72.0625 px wide, 24 px high, one line |
| Intentional spacer | Removed | 24 px retained |
| Dark theme surface | Light `rgb(246, 249, 252)` | Dark `rgb(25, 26, 25)` |
| Header artwork | Original geometry | Original geometry and black backing retained |

The fixed renderer passed six states: light, dark, light again, dark again at a
1000 px reader width, then dark and light at 640 px. The authored wrapper stayed
600 px wide. All four receipt amount occurrences, the heading, and footer were
retained. The footer stayed within the message surface.

All 11 mouse clicks were native Cocoa events and arrived with `isTrusted: true`.
Two complete history cycles grew the message from 971 px to 1073 px, then shrank
it back to 971 px. The expanded screenshot also visibly contains the earlier
history text. Retina device pixel ratio was 2.

The original control used the byte-identical renderer from base commit
`796c297` (the renderer was also unchanged at the verification checkout's recorded
`fccbace` revision). That recorded Git revision is the pre-fix base; the fixed
source hashes identify the changes tested in the working tree. It reproduced the narrow amount column, deleted spacer, and
light surface in dark mode. This establishes native sensitivity to the defect.

## Screenshots and machine-readable evidence

- [Fixed dark receipt](02-dark-wide.png)
- [Fixed light receipt at 640 px](06-light-640.png)
- [Expanded history](history-expanded-1.png)
- [Original renderer in dark mode](baseline/original-dark.png)
- [Fixed native result](result.json)
- [Original control result](baseline/result.json)
- [Provenance, source hashes, screenshot hashes, and commands](verification.json)

All twelve PNGs are direct `WKWebView.takeSnapshot` captures. Their signatures,
dimensions, sizes, and SHA-256 hashes were checked. The engine and executable
identity, native URL, macOS version, and Vite checkout are recorded in the JSON.
Workstation paths are replaced with `<checkout>` and local process IDs are omitted
before publication. These metadata redactions do not alter measurements, source
hashes, or screenshot hashes.

The fixture contains fictional merchant, contact, payment, and receipt details.
Only in the harness, the deliberately redacted merchant-logo URL is replaced by
a fictional local SVG with the same 96 by 156 px dimensions. The checked-in
receipt fixture is unchanged. The two generic header side images still load from
versioned public Stripe CDN URLs, so reproduction needs network access. No original
customer screenshots, mailbox files,
credentials, or compiled executables are included.

## Reproduce on macOS

Requires the locked npm dependencies and the macOS Swift compiler. The harness
uses Cocoa and WebKit directly. It does not require computer-use attachment,
Screen Recording access, or a browser automation session.

From the repository root, copy these three temporary test files:

```sh
cp docs/evidence/email-rendering/2026-10-17/harness.html apps/desktop/native-receipt-verify.html
cp docs/evidence/email-rendering/2026-10-17/harness.tsx apps/desktop/src/test/native-receipt-verify.tsx
git show 796c297:apps/desktop/src/components/HtmlMessage.tsx > apps/desktop/src/components/NativeBaselineHtmlMessage.tsx
npm run dev:web
```

Leave that Vite asset server running on `127.0.0.1:1420`. In a second terminal:

```sh
mkdir -p /private/tmp/dakia-receipt-native-results/baseline
swiftc -swift-version 5 -module-cache-path /private/tmp/dakia-receipt-swift-cache docs/evidence/email-rendering/2026-10-17/verify.swift -o /private/tmp/dakia-receipt-native-results/verify-webkit
/private/tmp/dakia-receipt-native-results/verify-webkit /private/tmp/dakia-receipt-native-results http://127.0.0.1:1420/native-receipt-verify.html
/private/tmp/dakia-receipt-native-results/verify-webkit /private/tmp/dakia-receipt-native-results/baseline 'http://127.0.0.1:1420/native-receipt-verify.html?baseline=1'
```

Require `pass: true` in both result files. The baseline additionally requires
`expectedBroken: true`. A successful process exit alone is insufficient. Require
the expected screenshots to exist and inspect the receipt and expanded history.
The source harness and Swift host are preserved alongside the captured evidence.

Afterward, stop only the Vite server started for this test and remove only the
three temporary files created above. The recorded run performed this cleanup.
Do not commit those temporary source files or the compiled harness executable.

## Automated regression checks

The complete frontend suite passed 365 tests across 38 files. The renderer suite
passed all 34 tests. TypeScript, formatting, and the production frontend build
also passed. The new receipt tests failed against the original implementation.
Coverage includes the actual redacted provider structure, mounted dark/light/dark
transitions, spacer and amount preservation, quoted tables, tracking pixels,
image-backed text, and QR-code contrast safeguards.
