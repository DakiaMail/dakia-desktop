# Mail safety programme: paused draft handoff

Paused on 22 October 2026 at the owner's request. Implementation, tests, provider work and native acceptance have stopped. This draft is not ready to merge or release. The original request to close all 69 testing gaps is not complete.

The tested source snapshot is `681dfc29e1cc782d363999cf572dc56be9498565d34f78ba5f0120187e0c70e9`, containing 996 governed files. This is a source-manifest digest, not a Git commit. The [preserved manifest](mail-safety-results/paused-681d-source-manifest-2026-10-22.json) identifies the tested inputs. This handoff and its supporting documentation were added afterward; no product fix was added during shutdown.

The branch starts from `796c297`. At publication, `main` also contains `e1da334` (Reader receipt layout and dark-mode fixes, PR #107). That change has not been merged into this tested snapshot. Reconcile it and revalidate the affected Reader behavior before merge; the existing results do not cover the combined version.

## Latest completed evidence

| Lane                | Result                                                                                                                            | Limits                                                                                                                                 |
| ------------------- | --------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| Full source         | Three consecutive unchanged 18-gate cycles: 54/54 passed, no retries. Separate strict fixture-feature Clippy passed.              | Does not establish native/provider acceptance.                                                                                         |
| Frontend            | 652 passed, two skipped per cycle; separate V8 coverage run also passed 652 with two skipped.                                     | Three files are skipped by the ordinary runner; dedicated artifact lanes exercise their respective paths. Skips are not passes.        |
| Rust                | Core library 775 passed, 13 ignored; 22 integration tests; desktop 227 passed; fixture-feature desktop 244 passed, three ignored. | The dedicated unsubscribe TLS runner executes those three ignored tests each source cycle. Other manual/ignored cases remain separate. |
| Script suite        | 296 passed per cycle.                                                                                                             | Unfinished private helpers are excluded.                                                                                               |
| Independent servers | Fresh GreenMail 2.1.14 TLS rejection and trusted IMAP/SMTP round trip passed. Dovecot 2.4.5 selective MIME round trip passed.     | Dovecot audit observed no full-raw fallback or mutating STORE. Fictional loopback mailboxes only.                                      |
| Fuzzing             | 31,068 MIME-budget and 104,594 attachment-metadata executions; no crash artifact.                                                 | Approximately 30 seconds per target.                                                                                                   |
| Dependency audit    | Terminal-confirmed exit 0; npm reported zero vulnerabilities; five vendored-provenance checks passed.                             | Ten allowed Rust unmaintained warnings remain. Earlier recorder failure retained.                                                      |
| Native build        | Current-source arm64 desktop/CLI compilation, resources and ad-hoc signature verified.                                            | Build/resource preflight only; no current-source native app launch or acceptance.                                                      |

Receipts: [three source cycles](mail-safety-results/gmail-schema3-typed-reader-three-run-source-2026-10-22.json), [deep lane](mail-safety-results/deep-681d-gmail-schema3-2026-10-22.json), [dependency audit](mail-safety-results/dependency-audit-681d-gmail-schema3-terminal-confirmed-2026-10-22.json), [shutdown evidence](mail-safety-results/paused-mail-safety-evidence-2026-10-22.json).

At draft-branch push time, GitHub separately reported 31 vulnerability alerts on the repository's default branch: ten high, fifteen moderate and six low. Those alerts have not been triaged here. The scoped local audit above does not establish that the default branch or every repository package has zero alerts; reconcile the different sources and dependency scopes before merge.

The MIME fixes and thirteen-fixture production MailService → SQLite reopen → Tauri command → React Reader artifact pipeline passed at the source boundary. Native WebKit acceptance of that expanded current-source corpus remains unrun. Its React artifact stage consumes real Rust output through a test API adapter, not live WebView IPC.

## Failures and incomplete runs

### Performance: unresolved, interrupted

The owned performance process was stopped during the third 200,000-message run. There is no completed three-run report or final performance verdict. Completed samples materially exceed the unchanged baseline.

| Messages | Baseline, ms | Run 1, ms | Run 2, ms | Run 3, ms |
| -------- | ------------ | --------- | --------- | --------- |
| 2,000    | 560          | 851       | 845       | 780       |
| 30,000   | 7,660        | 11,339    | 11,765    | 19,007    |
| 100,000  | 25,662       | 40,819    | 39,266    | 40,660    |
| 200,000  | 52,573       | 80,145    | 80,035    | Stopped   |

The cause is unproven. A concrete measurement candidate is the additional per-row account lookup/deserialization and provider-identity cleanup during fixture insertion. The timer also includes schema setup, fixture creation, synchronization, finalization and assertions. Host variance is substantial. Next: fixed phase measurements, observer-overhead checks and a controlled old/current frozen-source comparison on quiet hardware. Do not reset the baseline or adaptively retry until green. Earlier c56/a86 failures remain retained.

### Coverage: exporter failure, comparison pending

All instrumented Rust tests passed, but LLVM 23 crashed during full LCOV export. JSON and LCOV without functions exported successfully from the retained profile. Raw observations include 42,807/49,373 lines and 3,305/4,944 branches; the complete canonical report and baseline comparison are not accepted yet.

A reviewed [unapplied normalizer patch](mail-safety-results/draft-handoff-2026-10-22/coverage-normalizer.patch) preserves official line/branch data and reconstructs function-instantiation records from same-cohort JSON. Seven private tests and independent review passed. Four functions lack region mappings; some have branch observations. Their function mappings must remain explicitly unavailable while branches are retained. Next: revalidate retained objects, normalize the actual cohort, compare against `testdata/coverage/baseline.json`, and integrate the reviewed exporter/workflow fix. The baseline was not changed. Historical zero Rust branches means unavailable evidence, not complete coverage.

### Corrected failures and historical flakes

Earlier source attempts failed on a stale catalogue symbol and three TypeScript Element/HTMLElement errors. The corrected source subsequently passed all three full cycles. Earlier WebCrypto/timer and fixture child-cleanup oracle races were repaired; failed runs remain recorded. They are not hidden retries in the clean 681d cohort.

Private Gmail harness versions encountered Clippy, strict receipt-schema and test-oracle errors before provider access. The reviewed final v5 passed seven Rust tests, strict Clippy/build checks and fictional wrapper tests. Its live run never started. Earlier live-provider timeouts remain unexplained; later bounded historical successes do not erase them.

## Remaining work, in order

1. Investigate and resolve or explain the performance regression, then finish a predeclared comparison with the existing baseline.
2. Finish actual coverage normalization and ratchet comparison, then integrate a reproducible exporter/workflow fix.
3. Run the current schema-3 fictional provider/watch supervisor → Tauri event → frontend list → Reader flow, including restart and flag convergence.
4. Run current native attachment saving and interrupted-save quit/restart acceptance: exact bytes, warnings, permissions/quarantine, no final/partial file on failure, no automatic retry and exactly one explicit retry. Historical b14 native results cover bounded individual/Save All truncated-literal failures and retries, not the aggregate current-source requirement. No user-facing cancellation API exists.
5. Run the separate thirteen-fixture native Reader profile. Its UID 4–16 namespace must not be combined with translation, injection or compose fixtures. It deliberately forces full-raw fallback after selective attempts; do not call it thirteen successful selective fetches.
6. Run schema-3 forward-upgrade and fatal-startup acceptance. Support v0.4.4 forward upgrade only. Dakia now uses an accessible app-owned fatal recovery window, avoiding dependence on a person dismissing a macOS system alert. Historical c2 checks exercised it; current schema-3 checks remain unrun. Require preserved data, exit 1 and no inbox on failure.
7. Complete the new deterministic 30,000-message TLS fixture through the real connector → sync → Store → Reader MIME → CLI query path before native load acceptance. Private loopback tests passed 13/13 after multipart BODYSTRUCTURE corrections; the production smoke and final review remain unrun.
8. Complete native foreground responsiveness, account/window/offline/search/keyboard/notification workflows and resource attribution. The prepared WebKit collector supplies lower bounds or unavailable values, not an invented full-cohort resource pass.
9. Revalidate Gmail provider identity against the exact existing disposable test message using read-only operations when resumed. Schema-3 source tests cover X-GM-MSGID observation/missing/capability transitions, account boundaries and durable deduplication. Historical b14 live proof covers one bounded INBOX/Sent message through TLS/full fetch/Store/cache/reopen, not normal sync planning or native Reader. The queued new live run was cancelled; do not start it automatically.
10. Promote validated native helpers into reusable checkout-owned scripts with runtime manifest/approved-identity inputs. Finish offline tests, review and native acceptance. Private dated helpers and unfinished drafts are not a complete fresh-checkout programme.
11. Measure hosted Linux PR time/cost against ten minutes/USD 0.25. Complete release-specific Apple Silicon packaging, signatures, updater and supported-version checks when authorized. Ad-hoc fixture bundles do not establish Developer ID, notarization, stapling or public release acceptance.

## Catalogue, standards and capability limits

The catalogue declares 131 implemented references, 12 gaps, 17 manual-required scenarios and two unsupported policies. These are traceability labels, not a count of 69 closed gaps. Aggregate cases remain open when narrower slices passed.

- Gaps: `MS-B-004`, `MS-E-004`, `MS-J-004`, `MS-L-003`, `MS-G-013`, `MS-H-016`, `MS-I-013`, `MS-I-014`, `MS-K-016`, `MS-L-010`, `MS-L-011`, `MS-L-013`.
- Manual-required: `MS-F-003`, `MS-F-004`, `MS-G-003`, `MS-G-004`, `MS-B-017`, `MS-D-019`, `MS-F-012`, `MS-F-015`, `MS-G-014`, `MS-H-015`, `MS-J-011`, `MS-K-015`, `MS-K-018`, `MS-L-014`, `MS-L-015`, `MS-L-016`, `MS-L-017`. Some have bounded historical/current evidence in their lane reports; that does not close every aggregate assertion.
- Unsupported: `MS-D-011` non-atomic move fallback and `MS-J-012` external AI action policy. Do not add capabilities merely to turn these labels green.

The selected standards profile has 102 requirements: 86 tested, 11 intentionally tolerated and five unsupported. It is not an exhaustive RFC audit or complete IMAP4rev2 support. Broader MIME/native parity, aliases, configured-action egress, privileged-command isolation and longer soak/provider matrices remain partial. See the [gap index](mail-safety-remaining-gaps.md) and [standards profile](mail-safety-standards-profile.md).

## Decisions for resuming

- Reuse the approved app name, bundle identifier and app path. Prove the executable and live URL before native interaction.
- Credentials exist locally. Never commit `.env`, passwords, account addresses or raw Gmail observations. No new send is authorized by this handoff. Other-provider acceptance requires separately scoped disposable accounts; never use personal inboxes.
- Physical Mac sleep/wake was explicitly declined. Leave it untested; do not re-ask or interrupt the Mac.
- Downgrade is unsupported; restore the complete pre-upgrade backup. The published v0.4.4 binary cannot be retrofitted to recognize a future marker.
- No schedules, paid macOS runners, release signing/notarization/publication or production mailbox writes are authorized by this report. Review the draft before resuming the programme.

## Local artifacts outside the commit

These are local recovery aids, not portable checked-in infrastructure:

| Workstream                       | Preserved location                                                                                                           | State                                                                                                                              |
| -------------------------------- | ---------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| Coverage profiles/exports        | `/private/tmp/dakia-681d-global-workspace-coverage-20261022`, `/private/tmp/dakia-global-workspace-coverage-20261022-target` | Actual normalization/ratchet pending.                                                                                              |
| Reviewed coverage candidate      | `/private/tmp/dakia-rust-lcov-normalizer-private-20261022`                                                                   | Patch also preserved in this commit, unapplied.                                                                                    |
| Gmail proof v5                   | `/private/tmp/dakia-final-gmail-provider-identity-proof-v5-20261022`                                                         | Offline reviewed; no live run. Receipt SHA `bb7253df677848b99344f292e9f20f6db3c25ebb47eec1ccd56913f4019204ff`.                     |
| Native schema/attachment helpers | `/private/tmp/dakia-681dfc-schema3-native-helpers-20261022`                                                                  | Reviewed offline; native runtime unrun. Manifest SHA `f972dd6a462306ac07d3abf5c55b01d2b512d5af6a5cc0b47d81b9441fbbe946`.           |
| Build-only preflight             | `/private/tmp/dakia-681dfc-native-base-build-20261022`                                                                       | Runtime harness needs a fresh configuration/build. Receipt SHA `6e45163e6c6bb20fe23b4bc825e7b2f0a5eadc443b095ed8dd990b318f13b3ba`. |
| Thirteen-message native harness  | `/private/tmp/dakia-native-thirteen-reader-preparation-20261022`                                                             | Reviewed private harness; native run pending.                                                                                      |
| 30k TLS fixture                  | `/private/tmp/dakia-native-thirty-thousand-fixture-preparation-20261022`                                                     | 13/13 private tests; production smoke pending.                                                                                     |
| 30k smoke source                 | `/private/tmp/dakia-l003-production-smoke-source-20261022`                                                                   | Private test append never compiled/run.                                                                                            |
| Reusable MIME draft              | `/private/tmp/dakia-reusable-native-mime-corpus-candidate-20261022`                                                          | Partial and unreviewed; do not run.                                                                                                |
| Generic native helper draft      | `/private/tmp/dakia-reusable-native-helper-candidate-20261022`                                                               | Empty directory skeleton only.                                                                                                     |

No new live Gmail command, native app swap or native app run started before the stop. Unfinished private candidates have not been applied to the product source. The owned performance process was terminated; no follow-up tests should resume automatically.
