# Mail safety evidence lanes

Work was paused at the owner's request on 22 October 2026. See the [pause handoff](mail-safety-paused-handoff-2026-10-22.md) for the latest results, failures and remaining work. This programme is not complete or release-approved.

Current status, 22 October 2026: the latest source result is `681dfc29e1cc782d363999cf572dc56be9498565d34f78ba5f0120187e0c70e9`, 996 governed files. Its [three 18-gate source cycles](mail-safety-results/gmail-schema3-typed-reader-three-run-source-2026-10-22.json) passed all 54 gates without retries or manifest drift, with a separate strict native-fixture Clippy pass. This is source evidence only. It does not establish native WebKit, live-provider, package, release, performance, fuzzing, audit or global coverage acceptance.

The source result includes applied Gmail provider-identity/schema-3 work. Its [focused composition receipt](mail-safety-results/gmail-provider-identity-primary-composition-focused-2026-10-22.json) records 13 Gmail regressions and independent composition review. The [typed Reader correction receipt](mail-safety-results/provider-reader-thirteen-fixture-typescript-correction-2026-10-22.json) records the corrected same-profile full/selective MailService, Tauri mock-runtime and thirteen-fixture React Reader path. These focused results do not establish native WebKit or live-provider behavior.

Two earlier 996-file attempts remain visible as [a catalogue first-gate failure](mail-safety-results/gmail-schema3-source-catalogue-first-gate-failure-2026-10-22.json) and [a deterministic TypeScript gate-five failure](mail-safety-results/gmail-schema3-source-typecheck-gate-five-failure-2026-10-22.json). The completed 681d source cycles supersede them only for the named corrected source gates.

The [first 681d dependency-audit receipt](mail-safety-results/dependency-audit-681d-gmail-schema3-2026-10-22.json) is qualified because its recorder missed the script exit after the audit completed. The [terminal-confirmed audit](mail-safety-results/dependency-audit-681d-gmail-schema3-terminal-confirmed-2026-10-22.json) captured exit 0, npm's zero-vulnerability result and ten explicitly allowed cargo-audit unmaintained warnings. It is dependency-audit evidence only. Global Rust branch coverage is currently running, with no accepted metric yet.

The accepted fully verified native and provider checkpoint remains `b14fca648f7d0862f439fbb08890cef8085d025687e35d75322814d2f8dcca0e` across 961 governed files. Its [three source cycles](mail-safety-results/test-only-successor-b14-three-run-source-2026-10-22.json) passed all 51 gates without retries, failures or a changed manifest. The [bounded b14 native attachment and shared-profile receipt](mail-safety-results/native-b14-attachment-interruption-and-shared-profile-2026-10-22.json) passed named interruption, retry, account-isolation and simultaneous read-only CLI cases. The [b14 packaged-GUI forward-upgrade receipt](mail-safety-results/native-h016-faithful-packaged-gui-upgrade-b14-2026-10-22.json) passed one sealed fictional v0.4.4 forward-open path. The [b14 Gmail proof](mail-safety-results/gmail-bounded-inbox-sent-production-proof-b14-2026-10-22.json) passed one authorized read-only path across two physical copies of one existing message. Each is bounded evidence, not complete native, provider, updater or release acceptance.

The [earlier native snapshot-input receipt](mail-safety-results/source-snapshot-native-inputs-primary-focused-2026-10-22.json) records the old 961-file manifest limitation and the initial real 10/11 stale-inventory assertion failure. Its repaired focused contract total was 13/13. Those focused checks do not retroactively bind prior reports. The [component-equivalence receipt](mail-safety-results/b14-component-equivalence-from-0e1-2026-10-22.json) carries only its named `0e1` deep, loopback-performance and dependency-audit components, not native, provider, package or release evidence.

The prior evidence index is preserved byte-for-byte as [a dated history snapshot](mail-safety-results/mail-safety-evidence-history-before-4e6-current-reconciliation-2026-10-22.md). It retains the detailed 04b, a6, 09a5, a86, c2 and earlier failure chronology. See the [current gap index](mail-safety-remaining-gaps.md) for accepted limits and open aggregate scenarios.

## Repeatable execution reports

Run the source gates and write a machine-readable result:

```sh
npm run test:mail-safety -- --output /private/tmp/dakia-mail-safety-source.json
```

For three consecutive clean runs, add `--repeat 3`. The runner stops on the
first failure or unavailable command and preserves its non-passing status.
It records the revision, working-tree caveat, platform, command arguments,
exit codes, and timestamps. It does not store command output or mailbox data.
The source lane includes workspace tests, so it needs loopback access and the
worktree prerequisites. Ignored independent-server tests remain separate.
The report also hashes the paths and contents of the tested source tree,
including untracked implementation files. A modified checkout is therefore
identified by both its base revision and a source manifest digest. This is
local evidence, not a claim that hosted CI ran that revision.

Run the independent servers and bounded fuzzing explicitly:

```sh
npm run test:mail-safety:deep -- --output /private/tmp/dakia-mail-safety-deep.json
```

For storage or migration changes, run the opt-in SQLite recovery lane after the
full source gate:

```sh
bash scripts/run-sqlite-fault-injection.sh
```

This command uses a separate Cargo target directory and a bundled SQLite build
with `SQLITE_TEST`. It requires the exact WRITE and FSYNC extended error codes,
checks previously committed fictional mail, credentials, and send attempts
after reopen, and compares intact and manually torn copies of the same WAL. A
pass is evidence for those API-error and recovery boundaries only. It does not
simulate physical partial writes, disk exhaustion, loss before fsync durability,
or termination during commit, and it is not part of the ordinary pull-request
lane.

Native WebKit, credentialed providers, full packaged GUI and release upgrades,
and dependency audits need their own evidence. A source or deep-lane report
cannot establish those results. The separate manual performance lane establishes
only its declared loopback and hardware measurements. The catalogue report is a
declaration inventory and does not execute these commands.

## Mailbox identity and late work

An IMAP UID is meaningful only within an account, mailbox, and UIDVALIDITY.
For example, `INBOX` UID 42 at epoch 77 and UID 42 at epoch 78 can be two
unrelated messages. Dakia's existing stable row ID can be reused across that
change, so checking the row ID alone is insufficient.

The expanded safety boundary carries the captured epoch through UI requests,
native commands, authenticated SELECT, content publication, and local mutation
publication. Storage checks the receipt in the same transaction as its write.
This also protects against the bundled CLI changing the mailbox while the
desktop app waits for a network response. A process-local mutex alone cannot
protect two separate processes.

MOVE has two identities to validate. COPYUID supplies the destination epoch as
well as its UID. If the local destination cannot be matched to that receipt,
Dakia removes only the confirmed source representation and requests a fresh
sync; it does not overwrite a destination row from another namespace.

Atomic server MOVE is now required for moves. The older COPY/STORE/broad
EXPUNGE fallback could duplicate mail after a partial failure or expunge mail
marked deleted by another client. Unsupported MOVE returns a clear error.
Permanent deletion requires UIDPLUS and an exact UID EXPUNGE. A fully
recoverable COPY fallback remains a catalogue gap rather than an unsafe
compatibility promise.

Tests use the same stable ID at epochs 77 and 78 to check that old content,
attachments, flags, categories, classification, and events cannot contaminate
the replacement. The source catalogue names the exact executable tests and
continues to list cases that are not covered.

## Sending and recovery

| Outcome              | Meaning                                                                             | Retry behavior                                                                                 |
| -------------------- | ----------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| `not_submitted`      | The message was not submitted for delivery.                                         | The preserved draft can be retried.                                                            |
| `delivery_uncertain` | The connection ended without enough evidence to know whether delivery was accepted. | The current attempt is blocked from automatic repetition. Check Sent and recover deliberately. |
| `sent_copy_failed`   | SMTP accepted the message, but saving the Sent copy failed.                         | Never repeat SMTP merely to save the Sent copy.                                                |

Compose commits its exact draft, attachments, attempt ID, and sending state to
IndexedDB before starting the native send. A failed or aborted commit does not
start SMTP. The native account-scoped journal claims the attempt before SMTP
and survives restart; concurrent windows cannot claim that same attempt twice.
Only a definitely unsubmitted attempt can be released for retry. Terminal
uncertainty remains blocked if its recovery-state write fails.

The journal stores a payload hash and status, not message content. Draft
recovery is separate from the journal. Direct CLI sending still has no journal
across separate invocations; see [CLI limitations](cli.md).

The [sensitivity record](mail-safety-results/sensitivity-2026-10-17.json)
includes a representative broken implementation that skips the durable draft
commit. The first test version survived that mutation because its assertion ran
before the asynchronous send continuation. The corrected test flushes async
work while the commit is still blocked, detects the premature SMTP call, and
passes with the original production guard restored. This checks the ordering
that matters after a crash, rather than only checking that a storage helper was
called.

## Remote content policy

Remote email resources are blocked initially. Reader permission belongs only
to the current account, message, and mailbox epoch in the current Reader
session. Switching identity resets it. Explicit consent enables HTTP(S)
`img[src]` only. Media, SVG subresources, responsive image candidates, CSS
network resources, external stylesheets, and fonts remain blocked. Active
content stays blocked in both states.

Source regressions cover the sanitizer and Reader interaction path. A historical fixture WebKit record remains available in [native-mail-safety-acceptance.md](native-mail-safety-acceptance.md), but it is not current acceptance. The first current provider-to-Reader attempt is retained as a [failed native report](mail-safety-results/native-provider-reader-before-fix-2026-10-20.json): it observed zero fixture-server contacts before consent, then 35 post-consent requests after an unchanged-content parent rerender recreated the sandboxed iframe. It also exposed a missing main-window permission for the read-only `mail_rebuild_status` command.

The [current native report](mail-safety-results/native-provider-reader-current-2026-10-21.json)
passed provider arrival into the mounted Reader, consent reset without new
contacts, five history transitions, embedded SVG before and after consent,
native attachment saving and recovery cases, account-two Reader and compose
identity, malformed-message isolation, long-message controls, light and dark
control readability, reconnect recovery, and notification-listener handling.
Its allowed HTTP redirect failed with a WebKit network-connection-lost error,
so the report is `partial-failed` and no clean native acceptance is claimed.
Physical sleep and wake were explicitly waived to avoid interrupting the Mac.

The earlier [startup recovery alert record](mail-safety-results/native-startup-recovery-alert-access-2026-10-21.json)
remains unavailable automated UI evidence because the tool cannot inspect the
system-owned dialog. The subsequent [human-assisted check](mail-safety-results/native-startup-recovery-human-2026-10-21.json)
verified the expected text and absence of an inbox for all three future-version,
conflicting-schema, and corrupt-database alerts. The user clicked Quit Dakia in
each case. Twelve durable profile entries remained unchanged, excluding
transient SHM. All three processes incorrectly exited with status 0 instead of

1. A startup-only exit-status fix passes focused tests and independent review;
   rebuilt native dismissal and exit-status acceptance remain pending.

The [private Gmail diagnostic](mail-safety-results/gmail-read-only-recovery-private-diagnostic-2026-10-21.json)
verified the existing disposable message through read-only authenticated IMAP
without requesting SMTP submission or mailbox mutation. Its instrumentation
and LOGIN serializer factoring are absent from the frozen source, so that pass
does not establish final-source provider acceptance or resolve the earlier
frozen-source timeouts.

## Historical source baseline, 2026-10-18

[`gap-expanded-three-run-source-2026-10-18.json`](mail-safety-results/gap-expanded-three-run-source-2026-10-18.json)
records three consecutive, unchanged 15-gate source runs. The report identifies
the modified checkout by base revision `796c297dbc4ddde75a5bfc771a629d7524bc984a`
and source-manifest digest
`19097dead985f3c0d59fa4ae89cf788553675f667be54d9586805e2388e18dcf` across
717 files. It is local source evidence, not a hosted-CI or release result.

| Gate                                                                                            | Observed result                                                                    | Scope                                                                                                                                                                           |
| ----------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Full source iterations                                                                          | Three unchanged 15-gate runs passed                                                | Each run passed 749 Rust runtime tests plus 2 doc tests; 8 credentialed or independent tests remained ignored.                                                                  |
| Frontend and Node                                                                               | 491 frontend tests passed with 2 normal artifact-file skips; 165 Node tests passed | The normal frontend run skips artifact-dependent files only because their coupled runners execute them separately.                                                              |
| Coupled source artifacts                                                                        | Passed in every iteration                                                          | Realtime delivery, nine-fixture MIME Reader, attachment-command, and fictional-TLS mail-command artifacts ran as explicit commands.                                             |
| Static, fixture, inventory, release-script, and web-build checks                                | Passed in every iteration                                                          | TypeScript, Prettier, rustfmt, Clippy with warnings denied, validators, contract inventory, release-script tests, and web build.                                                |
| Deep independent-server and fuzz work                                                           | Not established by this source baseline                                            | A separate bounded deep lane is recorded below. It is not live-provider or vulnerability-absence evidence.                                                                      |
| Manual performance work                                                                         | Not established by this source baseline                                            | A separate bounded loopback measurement lane is recorded below. It is not provider-latency, native-responsiveness, long-soak, or regression-comparison evidence.                |
| Isolated non-GUI package update                                                                 | Not established by this source baseline                                            | A separate fictional-package report is recorded below. It does not establish a packaged GUI, production release candidate, or simultaneous desktop and bundled-CLI profile use. |
| Native, live-provider, packaged-release, current Rust advisory audit, and credentialed accounts | Not established by this baseline or the bounded companion lanes                    | No current native pass, live-provider round-trip, packaged GUI acceptance, current Rust advisory audit, or credentialed-account result is represented here.                     |
| Current npm lockfile audit                                                                      | Passed separately on 2026-10-20                                                    | [`npm-audit-2026-10-20.json`](mail-safety-results/npm-audit-2026-10-20.json) records zero vulnerabilities for the npm lockfile. It does not establish a Rust advisory result.   |

The [remaining-gap register](mail-safety-remaining-gaps.md) currently declares
129 implemented source scenarios, 17 gaps, 13 manual requirements, and 2
intentionally unsupported policies. MS-L-014 is a completed three-repeat manual
requirement, not a new source closure. Full programme acceptance and complete
standards conformance are not claimed. Product-wide Rust branch coverage and hosted
warm-cache PR time/cost acceptance remain unavailable evidence. Focused production
branch observations are recorded separately in the [scoped coverage report](mail-safety-results/scoped-rust-branch-coverage-2026-10-21.json):
TLS 7/7 tests observed 38 of 2,126 `mail.rs` branches; recovery 22/22 tests
observed 105 of 682 `storage.rs` branches. These separate fresh-profile runs
are not a merged product-wide percentage or coverage comparison.

## Bounded deep and performance evidence, 2026-10-18

The companion [deep report](mail-safety-results/gap-expanded-deep-2026-10-18.json)
passed on the same 717-file source manifest. Its loopback GreenMail and Dovecot
round trips used trusted TLS, and its untrusted-certificate case was rejected.
It also completed 43,985 MIME and 119,078 attachment fuzz cases, with each
target limited to 31 seconds and no crash observed. This is independent-server
and bounded fuzz evidence only. It does not establish compatibility with a
live provider or absence of vulnerabilities.

The companion [manual performance report](mail-safety-results/gap-expanded-performance-2026-10-18.json)
also passed on the same manifest. On an Apple M1 Max debug build, three
loopback scripted-IMAP runs measured 2,000-message snapshots at 0.912, 2.121,
and 0.886 seconds, and 200,000-message snapshots at 50.587, 51.360, and 51.152
seconds. The same lane completed 200 reconnect cycles and 100 Store-open
cycles, with file descriptors unchanged at 10 and resident memory increasing
by 1 MiB. These are bounded measurements. They do not establish provider
latency, native responsiveness, a long-running soak result, or a comparison
against a previous performance baseline.

The [isolated package-update report](mail-safety-results/k018-isolated-package-update-2026-10-18.json)
(`SHA-256 72f3fc47e043ab0d15b0e8b9b1f7375733f40052d8e728c6ecf9720629ef173a`)
passed a non-GUI Apple Silicon fixture lane. It verified the exact published
v0.4.4 input, confirmed that interrupted and tampered candidate downloads left
the isolated old app unchanged, and replaced only the isolated app copy with a
valid fictional signed candidate. The v0.4.4 bundled CLI created a fictional
disabled-account profile, and the candidate bundled CLI forward-upgraded that
profile while preserving its disabled account and message. Manual restoration
of the old bundle after a successful replacement also passed. The supported path and backup-based recovery steps are documented in [profile upgrade instructions](mail-profile-upgrades.md).

This result does not launch a packaged GUI or prove executable and webview
identity. It does not exercise simultaneous desktop and bundled-CLI access to
one profile, automatic rollback after replacement begins, candidate Developer
ID signing, notarization, stapling, Gatekeeper, public update reachability, or
installed-client behavior. The immutable published v0.4.4 binary also does not
reject a newer profile marker. That known limitation is separate from the
successful v0.4.4-to-candidate forward upgrade. Intel macOS remains waived and
untested.

The [CLI upgrade report](mail-safety-results/v044-forward-cli-upgrade-2026-10-21.json)
and [native-process report](mail-safety-results/v044-native-process-forward-upgrade-2026-10-21.json)
remain historical account, search, and startup observations. Their manually
seeded profile did not reproduce v0.4.4's starred-body cache layout. They cannot
establish preservation of the prior release's cached Reader body. The
[fixture correction record](mail-safety-results/v044-upgrade-fixture-correction-2026-10-21.json)
retains both original artifacts and their hashes. No production migration defect
was established from that invalid fixture, and no migration was changed to
accommodate it.

A faithful replacement uses the inspected published v0.4.4 layout, including an
enabled fictional account and the version-3 starred-body cache. Its sealed input
and exact cached body, HTML, flags, account, and UIDVALIDITY survived a current
CLI migration with SQLite integrity intact. This focused migration predates the
concurrent startup-lock correction. Packaged GUI display on that faithful input
and simultaneous desktop/CLI access remain pending. Downgrade is unsupported;
restoration uses the complete pre-upgrade backup.

The [current startup-lock baseline](mail-safety-results/startup-lock-three-run-source-installed-2026-10-21.json)
passed all 16 gates three consecutive times on unchanged source digest
`423a76f16559b42900d57c57f6875a04f0d7b56be1da2af0b79393de7fcda228`
across 758 files. Each passed 891 Rust runtime tests plus two doctests, 564
frontend tests, and 207 script tests. Eleven Rust ignores and two frontend
skips remain separate. The coupled realtime, MIME Reader, attachment-command,
and TLS mail-command checks also passed each iteration. The
[production startup race and its correction](mail-safety-results/concurrent-startup-fix-2026-10-21.json)
retain the original failed iteration; no failing-test retry counts as clean.
The initial dependency-install overlap is recorded separately in the
[environment triage](mail-safety-results/source-installation-overlap-2026-10-21.json).

The [same-source deep lane](mail-safety-results/startup-lock-deep-2026-10-21.json)
passed GreenMail and Dovecot round trips, 34,225 bounded MIME fuzz cases, and
107,208 attachment-metadata cases without a crash artifact. The
[current read-only Gmail recovery](mail-safety-results/gmail-read-only-recovery-startup-lock-2026-10-21.json)
verified the existing test message and exact body using an immutable current
probe, without sending or mutating the mailbox. Earlier LOGIN timeouts remain
unexplained. Neither source nor deep acceptance establishes the pending native,
packaged GUI, performance, or release-signing boundaries.

### Earlier investigation notes, retained for history

Both MIME findings have production fixes and permanent synthetic regressions:

- Invalid nested-message recovery states are rejected by the narrowly patched
  vendored parser. The regression passes in debug and optimized builds.
- Missing, repeated, cyclic, or orphan MIME part references are rejected before
  traversal work can grow without a bound. Nested messages retain their own
  local part-ID namespace. Debug and optimized graph regressions pass.

The first fuzz campaigns found a parser assertion and excessive memory growth;
those failed runs are retained in `deep-expanded-2026-10-17.json` and
`deep-final-2026-10-17.json`. They are not passes. The later
[accepted deep run](mail-safety-results/deep-accepted-2026-10-17.json) passed
GreenMail, Dovecot, 40,515 actual MIME-parser fuzz executions, and 102,513
attachment-metadata executions, with a 30-second limit per target. This is a
bounded smoke campaign, not evidence of vulnerability absence.

Three earlier full source iterations passed on source manifest `ad502e...`,
but an earlier frontend run on that same source failed the restored-compose-body
assertion. The original mock-contamination explanation was incorrect and is
retracted. The test observed the status button before the rich-text editor's
passive initialization effect had populated its HTML. The test now waits for
the restored body itself. A deliberate mutation that leaves the editor empty
still makes it fail, then the restored implementation passes. No production
change was needed for this test-readiness correction.

The [prior source evidence](mail-safety-results/source-accepted-pre-readiness-fix-2026-10-17.json)
keeps that earlier failure visible. A fresh three-run source baseline after the
readiness correction is recorded in
[source-readiness-fixed-2026-10-17.json](mail-safety-results/source-readiness-fixed-2026-10-17.json).
It passed three consecutive fresh iterations, all 12 commands per iteration,
with no failure and an unchanged source manifest.

The native run and accepted deep run used `ad502e...`. The only later source
change is the frontend test-readiness assertion. The
[build-input equivalence record](mail-safety-results/native-build-input-equivalence-2026-10-17.json)
compares all other 663 source inputs byte-for-byte and records identical digests.
The native result therefore continues to apply only to the unchanged historical
product code; it is not represented as a newly launched binary after a test-only
edit or as current native acceptance.

### Historical gates, retained for history

| Gate                         | Observed result                                                                  | Scope                                                                                                         |
| ---------------------------- | -------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------- |
| Full source iterations       | 584 Rust tests, 430 frontend tests, 147 Node tests passed per accepted iteration | Three independent-server cases ignored here; one artifact-dependent frontend case runs in the coupled command |
| Coupled realtime source test | Two focused Rust tests and one frontend test passed                              | Scripted provider cycle, production sink under MockRuntime, captured payload, production frontend adapters    |
| Static/build gates           | Passed                                                                           | TypeScript, Prettier, rustfmt, Clippy with warnings denied, web build                                         |
| Fixture/inventory gates      | Passed                                                                           | 21 governed fixtures; 50 commands, 12 native events, 11 frontend events                                       |
| Native WebKit                | Historical fixture result, not current acceptance                                | See historical fixture evidence under `mail-safety-results/native-2026-10-17/`                                |
| Dependency audit             | Historical audit result, not current audit evidence                              | RustSec and npm audit state can change and requires its own current lane                                      |

This historical snapshot does not change the current 20 gaps or 12 manual requirements or
convert native fixture work into a pass. Product-wide Rust branch coverage and hosted
warm-cache PR time/cost acceptance remain unavailable evidence. Focused production
branch observations are recorded separately in the [scoped coverage report](mail-safety-results/scoped-rust-branch-coverage-2026-10-21.json):
TLS 7/7 tests observed 38 of 2,126 `mail.rs` branches; recovery 22/22 tests
observed 105 of 682 `storage.rs` branches. These separate fresh-profile runs
are not a merged product-wide percentage or coverage comparison.

## Earlier baseline, retained for history

### Initial candidate before later MIME findings, 2026-10-17

These are local macOS Apple Silicon results for the modified checkout based
on `fccbace81192c5b5354f681e2feb2b66971e88cf`, not hosted CI or a published
release. Both execution reports have the same before/after source manifest:
`b4433ab96dca86ca6861d64d7d39a6489e5d30daabb022ddc563bba7d1751013`.

| Gate                                                                 | Result                        | Scope                                                                                                                                                                                |
| -------------------------------------------------------------------- | ----------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| [Complete source report](mail-safety-results/source-2026-10-17.json) | Passed three consecutive runs | 12 commands per run, without retries; source manifest unchanged                                                                                                                      |
| Workspace Rust tests and doctests                                    | 562 passed per run            | Includes core, CLI subprocess recovery, native command/realtime boundaries, and vendored updater; three external-server tests intentionally use the deep lane                        |
| Frontend                                                             | 430 passed per run            | One artifact-dependent test is skipped here and executed separately below                                                                                                            |
| Coupled realtime test                                                | Passed per run                | Scripted production IMAP cycle → production Tauri sink on MockRuntime → production frontend adapters and mounted App; artifact-dependent frontend test executed, not skipped         |
| Node suites                                                          | 144 passed per run            | Includes catalogue, fixture governance, CI selection, coverage, and release-script checks                                                                                            |
| Static and build checks                                              | Passed per run                | TypeScript, Prettier, rustfmt, workspace Clippy with warnings denied, web build                                                                                                      |
| Fixture and bridge inventories                                       | Passed per run                | 17 realistic fixtures; 50 commands, 12 native events, 11 frontend events                                                                                                             |
| [Deep report](mail-safety-results/deep-2026-10-17.json)              | Passed                        | GreenMail 2.1.14, Dovecot 2.4.5, and bounded fuzz targets on the same source manifest                                                                                                |
| Independent-server cases                                             | 3 passed                      | Strict untrusted-certificate rejection; GreenMail generic SMTP/Sent-copy/IMAP delivery; Dovecot IMAP sync and database reopen                                                        |
| Bounded fuzzing                                                      | No crash                      | MIME-budget target: 118,689 executions; attachment-metadata target: 116,075; 30-second limit each. This is a smoke campaign, not full MIME-parser fuzz assurance                     |
| Dependency audit                                                     | Passed                        | Refreshed RustSec and npm audits. Existing supported-macOS reachability exceptions remain checked; seven allowed unmaintained Rust warnings remain; npm reports zero vulnerabilities |
| Independent source review                                            | Approved                      | Source correctness/security review; excludes release acceptance                                                                                                                      |

The source runner completed in 230, 158, and 156 seconds respectively on this
host. These local full-suite timings do not measure the selected Linux PR
workflow's billable time or cost. Hosted warm-cache budget acceptance remains
unmeasured; no branch-protection, schedule, paid runner, or secret was enabled.

The [pre-baseline report](mail-safety-results/source-prebaseline-2026-10-17.json)
is retained as a failed integration attempt. It exposed missing artifact-lane
setup and records source changes during execution. It is excluded from the
three clean baseline runs.

At this earlier baseline, the catalogue contained 151 scenarios: 68 with source evidence,
72 explicit coverage gaps, and 11 manual requirements. It is a programme
backlog and traceability inventory, not a claim that all approved scenarios
are implemented. Full programme acceptance remains incomplete. In particular,
live Wry/WebKit, packaged upgrades, sustained fault/performance campaigns,
credentialed-provider acceptance, complete standards profiles, and the
remaining deterministic catalogue cases need further evidence. Rust branch
observations remain unavailable; no coverage baseline was inflated or reset.

The [current performance comparison](mail-safety-results/startup-lock-performance-comparison-2026-10-21.json) is **regressed**, not passed. Three quiet repeats on the same stated hardware and Rust profile produced a 2,000-message median of 842 ms against 654 ms, a 28.75% increase above the 20% investigation threshold. The 30,000, 100,000, and 200,000-message cases remained within the threshold at +6.08%, +5.07%, and +3.71%. The bounded 200-reconnect/100-Store-cycle soak kept file descriptors at 10 and resident growth at 1,261,568 bytes. The source fingerprint was unchanged. Investigation remains open; the earlier successful performance report does not establish acceptance for the current source.

The first storage branch refresh was rejected because the exporter combined an older executable’s mappings with current mappings despite fresh raw profiles. Its [original record](mail-safety-results/scoped-storage-startup-branch-coverage-2026-10-21-stale-objects.json) is preserved byte-for-byte. The [accepted clean refresh](mail-safety-results/scoped-storage-startup-branch-coverage-2026-10-21.json) uses normal cleanup and proves one current build directory and one storage-source entry in each export. The separate cohorts pass 9 startup-lock tests with 89 of 724 storage branches observed, and 22 storage-fault tests with 124 of 724 observed. These percentages are not combined and do not establish product-wide coverage.

[Latest packaged CSP and attachment checkpoint](mail-safety-results/native-packaged-csp-bridge-safe-checkpoint-2026-10-21.json) records the unchanged-source partial native result, the blank Compose failure, exact attachment checks and cleanup, and the acceptance cases still unrun.
