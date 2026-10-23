# Mail safety evidence lanes

Current status, 22 October 2026: the test-only b14 successor across 961 governed files passed [three source cycles](mail-safety-results/test-only-successor-b14-three-run-source-2026-10-22.json), with 17 gates in every cycle, no retry, failure or source-manifest change.

The live working tree is now `9a9951887997397535d1c607836b69c3a19b2bc4880236a40c24e6f5d400cee2` across the same 961 governed files. It adds only the focused [configured-AI egress test receipt](mail-safety-results/ms-j-004-configured-ai-egress-primary-focused-2026-10-22.json): five focused tests and `cargo fmt` passed. Production AI behavior and the disabled desktop feature are unchanged; the receipt makes no LocalCommand operating-system sandbox claim and does not close aggregate `MS-J-004`. A combined full source run is pending, so b14 remains the latest three-run, native and provider-verified checkpoint. The [bounded b14 native attachment and shared-profile receipt](mail-safety-results/native-b14-attachment-interruption-and-shared-profile-2026-10-22.json) passed individual and Save All interruption/retry behavior, account-isolated routing and eight concurrent read-only CLI operations against the live desktop profile. The [b14 packaged-GUI forward-upgrade receipt](mail-safety-results/native-h016-faithful-packaged-gui-upgrade-b14-2026-10-22.json) passed one sealed fictional v0.4.4 profile with its unread starred cached message visible in the Reader and a normal exit. These native receipts remain bounded: they do not establish full native, provider, package or release acceptance. The [component-equivalence receipt](mail-safety-results/b14-component-equivalence-from-0e1-2026-10-22.json) carries only byte-identical 0e1 deep, loopback-performance and dependency-audit inputs. c2/956 remains the latest completed deep checkpoint and retains its exact MIME, attachment and terminal-restart native slice. A test-only successor at 9107367/956 has been applied to the primary checkout with three changed paths; its 24 focused storage-fault tests and 13 native shared-profile helper tests passed, but no complete successor source, deep or native lane has run. The [successor progress receipt](mail-safety-results/h016-k018-test-only-successor-progress-2026-10-22.json) records the exact application and limits. c2/956 completed three standard noninteractive source cycles, 17 gates each, without a recorded retry or failure. The [c2 source and application receipt](mail-safety-results/c2-source-application-and-native-pending-2026-10-22.json) records its source hashes and the exact three applied paths: the fatal-exit bridge, local AI child handling, and its privacy regression test. A same-identity [c2 targeted native recovery check](mail-safety-results/native-targeted-recovery-c2-2026-10-22.json) passed regular Cmd-Q and the three named fatal profiles. A separate [c2 MIME, attachment and terminal-restart slice](mail-safety-results/native-mime-attachment-terminal-restart-c2-2026-10-22.json) passed the exact 4→8→5→8→6→8→7→8 MIME sequence, attachment saves with 0600 mode and quarantine, one user-initiated fictional Send action with Sent-copy failure UI, watcher delivery, and one same-profile relaunch. These are bounded slices, not complete native acceptance. The 09a5 [fatal-startup slice](mail-safety-results/native-fatal-recovery-09a5-partial-2026-10-22.json) retains the prior corrupt-profile Cmd-Q exit-0 failure as sensitivity evidence. The [current c2 deep lane](mail-safety-results/c2-deep-independent-servers-and-fuzz-2026-10-22.json) passed one GreenMail TLS-rejection and trusted SMTP/IMAP roundtrip, Dovecot 2.4.5 selective metadata/plain/HTML fetches without raw fallback or STORE, and two 30-second fuzz targets over 76 seeds with no crash artifact. It excludes native, provider, package, performance, audit and release acceptance. See the [current gap index](mail-safety-remaining-gaps.md) for the remaining limits.

The earlier 04b checkpoint remains distinct historical evidence: its source passed [three complete source cycles](mail-safety-results/staged-logout-full-list-three-run-source-2026-10-22.json), [independent servers and bounded fuzzing](mail-safety-results/staged-logout-full-list-deep-2026-10-22.json), [the retained-baseline performance comparison](mail-safety-results/staged-logout-full-list-performance-2026-10-22.json), and [one read-only Gmail full-LIST observation](mail-safety-results/gmail-full-list-production-proof-04b-2026-10-22.json). Its [application receipt](mail-safety-results/source-application-04b-2026-10-22.json) records the earlier staging-to-primary equivalence. The [same-identity native 04b checkpoint](mail-safety-results/native-provider-reader-04b-partial-2026-10-22.json) is partial-failed and retains bounded native privacy, Reader, attachment, ACL, account and held-sync observations plus its terminal recovery defects.

The sealed a6 candidate also passed [three source cycles](mail-safety-results/a6-three-run-source-candidate-2026-10-22.json): 51 of 51 source gates completed without a recorded retry or failure. The [input comparison](mail-safety-results/a6-component-evidence-equivalence-from-04b-2026-10-22.json) limits the related 04b evidence continuity to named core, deep, performance and audit inputs. A6 is not applied to the primary checkout, so this candidate result does not establish a primary native, provider, package, release, deep, performance or audit result.

A separate future recovery-window candidate [failed and required a provenance amendment](mail-safety-results/future-recovery-window-source-provenance-amendment-2026-10-22.json). Its private nonstandard runner stopped at Clippy `type_complexity`; its manual source manifest also omitted `apps/desktop/fatal-recovery.html` from the standard snapshot scope. The repaired 4e69 standard run was interrupted before completion by a later normal-exit-status defect and is not a pass. The corrected 09a5 candidate then passed the standard noninteractive source lane and was applied, as recorded above. The earlier PTY interruption remains historical: an optional AI privacy test inherited stdin through Tokio LocalCommand.output and waited for fixture-process EOF.

Within that checkpoint, a [faithful packaged-GUI forward upgrade](mail-safety-results/native-h016-faithful-packaged-gui-upgrade-04b-2026-10-22.json) passed for one sealed fictional v0.4.4 profile: the exact unread starred cached message was visibly opened in the `tauri://localhost/` Reader, the profile moved out of its normal Application Support location, and the process exited normally. This bounded H016 slice does not establish downgrade, current startup alerts, same-profile restart, signing, notarization, updater publication or release acceptance.

At that historical 04b checkpoint, two fatal-startup fixture cases passed: conflicting send attempts and a corrupt database each showed their expected recovery text to the user, showed no inbox, exited with code 1 after Quit Dakia, and left durable profile files unchanged. The [04b fatal-startup receipt](mail-safety-results/native-startup-fatal-exit-04b-2026-10-22.json) preserves their separate hashes and limits. It does not establish same-profile restart, general recovery behavior, release acceptance, or any provider behavior.

The [04b same-profile relaunch checkpoint](mail-safety-results/native-same-profile-relaunch-04b-partial-2026-10-22.json) is partial: it visibly recovered the account-locked terminal draft with Send disabled and showed the UID 6 body without attachments after a normal relaunch exit. It did not produce runtime attestation, a seal, post-restart durable-state evidence or the UID 7 leap-second case because the profile inventory helper rejected its `lsof` result. It records no SMTP resend.

Current status is maintained in [Remaining mail safety testing gaps](mail-safety-remaining-gaps.md).

The [a86 source baseline](mail-safety-results/remaining-gap-closure-three-run-source-snapshot-inventory-and-starred-retention-2026-10-22.json) passed all 16 source gates in three consecutive, unchanged runs on digest `a86feeae88ffe172c8697a84f1d29afa123856661ee3765ca3307562c410193c` across 940 files. Its [detail receipt](mail-safety-results/remaining-gap-closure-source-snapshot-inventory-and-starred-retention-detail-2026-10-22.json) records 1,008 workspace Rust runtime passes, two doctests, six coupled Rust assertions, 615 ordinary frontend passes, 11 coupled frontend tests, and 246 script passes per run. Eleven ignored Rust tests, two frontend test skips, and three artifact-gated frontend files remain separate. All 48 gates exited successfully without a scheduled retry or observed flake. The web builds retained one fixture-environment warning and five Bergamot Node-module externalization warnings per run; those warnings do not establish WebKit translation-worker behavior.

The [4cf deep lane](mail-safety-results/remaining-gap-closure-deep-snapshot-inventory-and-smart-membership-2026-10-22.json) passed GreenMail and Dovecot loopback checks plus two bounded 31-second fuzz campaigns with 28,826 MIME and 109,436 attachment executions over 75 seeds and no crash artifact. The [component-equivalence receipt](mail-safety-results/final-snapshot-inventory-and-starred-retention-component-evidence-equivalence-2026-10-22.json) verifies that the 4cf core, CLI, Cargo, vendor, fuzz, MIME-fixture, server-runner, fuzz-runner, and nine audit inputs are byte-identical at a86. It does not carry forward a whole-source, frontend, native, provider, or package result. The older cargo-fuzz infrastructure and tool-path failures remain historical failures. The 64 MiB sanitizer quarantine limits temporal use-after-free sensitivity.

The [current bounded Gmail provider proof](mail-safety-results/gmail-bounded-inbox-sent-production-proof-b14-2026-10-22.json) passed one authorized read-only run for one known existing message represented by distinct physical INBOX and Sent copies. It verified certificate-checked TLS, wrapper LIST resolution, full fetch, parsing, epoch-fenced Store/cache publication, reopen, one display representative, pre- and post-run flag invariance, and an unchanged credentials file without SMTP or mailbox mutation. Its runner was compiled from 660 b14 core, CLI, Cargo and vendor inputs and embeds the b14 source digest. This does not establish Dakia's production LIST parser, normal sync planner, missing or unusable RFC Message-ID and X-GM-MSGID identity behavior, label convergence, broad reliability, other providers, native Reader behavior or sending. Earlier provider timeouts and harness failures remain retained and unexplained. The [a86 proof](mail-safety-results/gmail-bounded-inbox-sent-production-proof-a86-2026-10-22.json) remains historical evidence for its own source identity.

The [a86 performance lane](mail-safety-results/remaining-gap-closure-performance-snapshot-inventory-and-starred-retention-2026-10-22.json) is **regressed**, not accepted. The 2,000-message median was 705 ms against a 560 ms baseline, a 25.89% increase beyond the 20% threshold. The 30,000, 100,000, and 200,000-message comparisons remained within threshold, and the 200-reconnect/100-store-cycle soak held file descriptors at 10 with 1,523,712 bytes of resident growth. This loopback measurement does not establish provider latency, native responsiveness, or sustained process memory behavior.

The same-identity a86 packaged native run has a [partial-failed checkpoint](mail-safety-results/native-provider-reader-a86-partial-checkpoint-2026-10-22.json), an [earlier follow-up](mail-safety-results/native-provider-reader-a86-partial-followup-2026-10-22.json), and a [later H016 and UID 6 companion](mail-safety-results/native-provider-reader-a86-h016-and-uid6-diagnosis-followup-2026-10-22.json). Bounded checks passed for packaged ACL denial, controlled remote-content permission, several native Reader and attachment flows, and the sealed v0.4.4 forward-upgrade fixture with visible subject, body, unread filter, star, `tauri://localhost/` binding, and normal exit. The native result remains partial-failed: external STAR duplicated a message between Smart Seen and Starred; the terminal sent-copy-failure Compose flow had no accessible recovered draft; UID 6 exposed contradictory GreenMail leaf MIME headers that Dakia accepted, yielding a blank Reader body and false attachments; and the planned UID 7 replay did not run. The separate [conflicting-send-profile alert receipt](mail-safety-results/native-conflicting-send-profile-startup-quit-a86-2026-10-22.json) records accepted human inspection, Quit, exit 1 and unchanged durable files. At that historical a86 checkpoint, the corrupt-profile scenario had not run. Notification alert acceptance, the full resource cohort, and release acceptance remain open.

The 04b source repairs are now applied to primary through the recorded path/content-equivalent application. That carries the named staging source evidence only. The 04b checkpoint records the bounded native slices in its own receipt and does not carry forward a86 native, provider or release acceptance. A separate Dovecot 2.4.5 staging probe at `/private/tmp/dakia-dovecot-selective-mime-evidence-20261022.json` observed production metadata plus plain and HTML leaf fetches, cache/reopen behavior, no raw fallback, and no mutating STORE. It remains bounded staging evidence, not a final native or release result.

Dated reports retain their original source fingerprints; older uses of “final” or
“current” describe that report’s checkpoint, not later working-tree changes.

The [historical 1a native replay](mail-safety-results/native-corrections-provider-reader-2026-10-21.json) completed with a partial-failed result. Actual offline WebKit translation passed with zero controlled remote-resource contacts, along with mounted arrival and flag events, repeated history resizing, eleven mailto cases, colliding-account detached Reply/Forward, read-only desktop/CLI sharing, unsafe saving, bulk saving, real disk-full failure and successful retry, and long-message scrolling. It exposed a further Smart Seen-row omission and an evidence-name collision in the checked-in attachment helper. The private reviewed helper correction completed the filesystem acceptance; durable source corrections and a final native replay remain required. All owned fixture services stopped, the disk image detached, and original Downloads restored.

The [fixed fatal-startup acceptance](mail-safety-results/native-startup-fatal-exit-fixed-2026-10-21.json) passed for the future-version profile: the user confirmed the expected alert and no inbox, clicked Quit Dakia, the actual OS process exited 1, and profile bytes were unchanged. The earlier two other alert-text confirmations remain separate and their exit checks were not rerun on this build.

The [historical source Gmail CI entrypoint](mail-safety-results/gmail-readneutral-ci-auth-2026-10-21.json) passed read-neutral IMAP and SMTP authentication/status in 1,822 ms, with clean exit and temporary-state removal. It never fetched or searched a message, sent, appended, or changed flags. It does not close the earlier exact-message recovery timeout.

The [three read-only Gmail recovery observations](mail-safety-results/gmail-three-readonly-recovery-observations-2026-10-21.json) passed the same existing message's production recovery path through header sync, exact search, full fetch, hydration and cache in 10.048, 14.870 and 13.508 seconds. Each completed 22 recorded phases, left credentials and environment files unchanged, and requested no mailbox mutation or message submission. The report also confirms that the 657 relevant core, CLI, Cargo and vendor inputs remain byte-identical to the c56 inputs used by the tested executable. The earlier approximately 45-second recovery timeouts remain unexplained. This is one disposable Gmail account and one exact path, not broad provider reliability, label convergence or native Reader acceptance. The later a86 bounded B004 proof is recorded above and remains separate from this historical three-observation receipt.

An [earlier source baseline](mail-safety-results/native-corrections-three-run-source-2026-10-21.json) passed all 16 gates in three consecutive runs without retries on unchanged digest `1a27848c0142d4b2f51439fc77caeae076acc9fd7a640b66569aa0b4d67376e6` across 761 files. Each run passed 892 Rust runtime tests plus 2 doctests, 565 frontend tests, and 213 script tests. Eleven ignored Rust cases and two frontend skips remain separate. This baseline includes the Smart retained-row fix, the fictional translation-model scope correction, and the checked-in native attachment fault helper. Its historical 1a native replay is recorded above. Earlier source and runtime reports below retain their original identities and limitations.

The [native replay of the completed source checkpoint](mail-safety-results/publication-detail-native-provider-reader-2026-10-21.json) remains partial. It passed post-mounted arrival, read-neutral flags, allowed redirects, five history transitions, all 11 physical mailto cases, reply/forward resource privacy, colliding-account detached workflows, simultaneous desktop/CLI use, malformed-message isolation, long-message scrolling, symlink-safe saving, and actual HFS+ disk-full failure followed by restore and successful retry. It found a real duplicate Smart-row bug and a fixture translation-model permission mismatch. Those corrections require a fresh source baseline and native replay. All test mounts were detached and owned fixture processes stopped before source editing resumed; earlier failures are retained.

The final [quiet performance comparison](mail-safety-results/publication-detail-performance-comparison-2026-10-21.json) passed all four scales across three repeats on unchanged final source. Medians were 628 ms for 2,000 messages, 7,783 ms for 30,000, 26,575 ms for 100,000, and 54,982 ms for 200,000, all within the unchanged 20% threshold against the accepted 20 October baseline. The bounded 200-reconnect/100-Store-cycle soak kept file descriptors at 10 and resident growth at 1,327,104 bytes. The [preparation record](mail-safety-results/publication-detail-performance-preparation-2026-10-21.json) records the generated-artifact cleanup; the original regressed run and the controlled 31 ms query-only effect remain separate evidence. Compiler-artifact variance is not a proven causal explanation, and this lane does not establish native UI responsiveness.

The [bounded Gmail lifecycle diagnostic](mail-safety-results/gmail-read-only-recovery-lifecycle-diagnostic-2026-10-21.json) observed child exit 1 after 49,858 ms, without a signal or parent timeout. No child stage marker was captured. The overall timeout also covers temporary SQLite initialization and encrypted credential persistence before the first IMAP marker, so it does not identify a provider or network fault. The independent read-only IMAP control passed, but final-source recovery remains unresolved. No further provider attempt or mailbox mutation was made. An accidental report replacement was recovered byte-for-byte against its verified original hash; the new diagnostic has a distinct filename.

The final [clean storage branch refresh](mail-safety-results/publication-detail-storage-branch-coverage-2026-10-21.json) passed three separately instrumented cohorts on this same final source: startup locking 9/9 tests, 89/728 branches observed; storage faults 22/22 tests, 124/728; realtime publication 9/9 tests, 130/728. Normal cleanup and current-object maps exclude stale executable mappings. These observations are separate scopes and are neither added together nor presented as product-wide coverage.

The latest completed source checkpoint is [publication-detail-three-run-source-2026-10-21.json](mail-safety-results/publication-detail-three-run-source-2026-10-21.json): all 16 gates passed in three consecutive runs without retries, on unchanged digest `2a4e14731ba246806f3700b020ff7c1ec4afdbce0ceea6466fcfa46cb41d475f` across 758 files. Each run passed 892 Rust runtime tests plus 2 doctests, 564 frontend tests, 207 script tests, and all four coupled artifacts. Eleven ignored Rust cases and two frontend skips remain separate. The [matching deep lane](mail-safety-results/publication-detail-deep-2026-10-21.json) passed independent GreenMail and Dovecot round trips and bounded fuzzing with 37,273 MIME cases and 95,996 attachment-metadata cases. Earlier checkpoints below are historical and do not override this result.

At that historical checkpoint, the immutable final-source Gmail recovery [timed out](mail-safety-results/gmail-read-only-recovery-publication-timeout-2026-10-21.json); the [independent read-only control](mail-safety-results/gmail-read-only-recovery-controls-2026-10-21.json) authenticated successfully but did not resolve that timeout. Earlier successful probes remain separate evidence. The later bounded a86 Gmail proof is recorded above. No Gmail send or mailbox mutation was requested.

The mail safety catalogue is checked on applicable pull requests. It keeps
source tests, independent-provider work, native behavior, and release evidence
separate so that an unavailable environment cannot appear as a pass.

| Lane           | Evidence it can establish                                                      | It cannot establish                                          |
| -------------- | ------------------------------------------------------------------------------ | ------------------------------------------------------------ |
| `pull-request` | deterministic Rust, TypeScript, fixture, protocol, and contract tests          | WebKit behavior, installed builds, or a real provider        |
| `manual`       | bounded fault, state-model, performance, and independent-server work           | an ongoing release or production mailbox compatibility claim |
| `provider`     | explicitly authorized, read-neutral authenticated discovery and SMTP auth/QUIT | sending, mailbox writes, or general provider compatibility   |
| `native`       | actual macOS WebKit, native menu, filesystem, and packaging interaction        | a substitute for source-level regression tests               |
| `release`      | signed, packaged, upgrade, and deployment acceptance                           | a reason to omit a failed source test                        |

The `implemented` field in the catalogue means a source reference exists. A
lane result is evidence only after the relevant command succeeds for the
reported revision. Catalogue `gap`, `manual-required`, and `unsupported`
entries are visible in the report and cannot produce a pass.

The dependency audit is a change-selected pull-request step: it runs only for
lockfile changes, compiles the pinned `cargo-audit` version with Dakia's pinned
Rust 1.89 toolchain, refreshes the Rust advisory database, and audits the npm
lockfile without lifecycle scripts. It uses no configured credentials and has
no schedule. Its network requests are the advisory and npm audit services.

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

## Current source baseline, 2026-10-18

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
