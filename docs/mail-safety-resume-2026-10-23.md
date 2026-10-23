# Mail safety programme continuation, 23 October 2026

The owner authorized resuming draft PR #113. Work remains in progress. The PR stays draft; no merge, release, signing, notarization, schedule or production mailbox write is authorized or performed.

The clean starting checkout was detached at `e1da334`. The existing programme head was `d813a25`, checked out in another worktree with private files and local assets. The continuation uses `f/mail-safety-confidence-resume` and will update the same remote PR branch. The older worktree, credentials, caches and evidence remain intact.

## Combined source

Merge commit `12383a0` includes main’s receipt layout and dark-mode corrections, the reviewed inert-document renderer, remote-content consent policy and all programme changes. The combined Reader suite passed 51 tests and the realistic-fixture validator passed 11 tests. Receipt images start blocked; consent permits image `src` only, while CSS network backgrounds remain blocked. Current and quoted-history bodies are classified independently. The regression exercises expand, collapse and expand again.

Initial merge-check attempts exposed obsolete remote-image expectations and test assertions against the wrong history wrapper. These failed attempts are retained in `/private/tmp/dakia-resume-reader-merge-20261023.log`, `...-corrected-...`, `...-policy-...` and `...-final-...`; the successful focused run is `...-verified-...`. They are corrected integration attempts, not clean final-source cycles.

The full frontend preflight passed 660 tests, with two skips and three artifact-dependent files skipped by the ordinary runner. The separate artifact lanes exercise their own paths. The workspace preflight passed, including 778 core-library tests with 15 explicit ignores, 227 desktop tests, CLI/integration suites and doctests. Sources were still changing in independent helper work, so these are preflight checks rather than a frozen final-source receipt.

## Coverage

Commits `33e40cd` and `ad6c045` integrate paired JSON/line-and-branch exports with source, profile, object and output receipts. The normalizer preserves all non-function LCOV records and lists unavailable function mappings explicitly. The actual LLVM tool path and proxy/receipt/normalizer integration were checked locally.

The [retained 681d normalization](mail-safety-results/rust-coverage-normalization-681d-2026-10-23.json) records 42,807/49,373 lines, 3,305/4,944 branches and 5,526/11,760 instrumentation instantiations. Four function mappings remain unavailable. Rust line and frontend ratios pass the unchanged baseline; Rust branch comparison is unavailable because the old baseline has no branch observations, and Rust function comparison is unavailable because its legacy metric is incompatible. This is historical-cohort evidence, not current-source coverage. No baseline was reset. The hosted manual workflow has not run.

## Performance

Provider lookup and identity cleanup SQL during fixture insertion was a material contributor to the slowdown. A fixed interleaved A/B preserved both executable hashes and all twelve samples. The 2k median changed from 723 to 539 ms; 30k changed from 10,966 to 8,509 ms. Insertion fell by about 29–32%; sync time was roughly unchanged. This is a causal fixture-insertion experiment, not the final current-source comparison. Host load was unusually high, and the dependency/source cohort later changed.

The production staged finalization path now also resolves provider classification once per account and uses an account index for observation cleanup. Regression tests preserve mixed-account boundaries, provider changes, corrupt/missing/mismatched account data, flags and staged publication. The existing baseline remains 560 / 7,660 / 25,662 / 52,573 ms for 2k / 30k / 100k / 200k. Final fixed three-repeat comparison remains pending, without adaptive retries. The [causal investigation](mail-safety-results/performance-provider-batch-causal-investigation-2026-10-23.json) preserves all samples and both experiment versions.

## Dependencies

The [scope reconciliation](mail-safety-results/dependency-alert-scope-reconciliation-2026-10-23.json) distinguishes GitHub’s 31 default-branch alerts from the PR dependency inputs. Sixteen root npm alerts were already fixed in the PR. The analytics lockfile still had vulnerable development dependencies, and three resolver advisories were reachable in the macOS app despite no matching RustSec vulnerability result.

The compatible Hickory family is updated to 0.26.2, the newly yanked Windows-only dependency to 5.4.3, and analytics Vitest/Wrangler to fixed versions. Root and analytics npm audits report zero vulnerabilities. The canonical Rust audit passes with five vendored-provenance checks and ten allowed unmaintained warnings. Analytics tests, type checks and a deployment dry run passed; nothing was deployed. The receipt retains the first mixed-Hickory compile failure and sandbox-restricted test attempts.

## Provider and native acceptance

A fresh fictional 30k loopback integration reached the actual TLS connector, sync, Store, Reader MIME cache, reopen and CLI search. That first scoped run is not a final source-bound native acceptance result. Its reusable runner is undergoing lifecycle/provenance review before a frozen repeat.

The reusable Gmail proof is limited to the existing authorized test ID `45bacd384f0c4a74893f14e4fea23d9a`. Its runner is compiled without credentials, copied and hashed before credentials are loaded, and uses read-neutral IMAP only. The wrapper and Rust contract tests pass offline. No resumed live Gmail check has run yet.

Native helper preparation revalidated the exact approved app name, bundle identifier and path from its sealed identity record. No new app identity was introduced. Schema-3 upgrade/fatal, attachment quit/restart/retry, the thirteen-fixture Reader flow, foreground and resource acceptance remain pending. Current-source unit and integration gates come first. Physical Mac sleep/wake remains declined, and downgrade remains unsupported with complete backup restoration.

The catalogue remains a traceability inventory. The programme and the original 69-gap request are not complete. Unsupported capabilities, unavailable resource attribution, ignored cases and release-only checks do not count as passes.

## Frozen-run and hosted failures retained

The first resumed full cycle passed all 18 commands, but its source guard failed because the hosted-CI test correction landed during execution. The before digest was `e4fcf7089175e76276b0d6db14eadb9bc17ff922dc4a4f9a7ce31dfdaa0a5948`; after was `670652ff0da3121eabc3f2ede32d2089828588da85904de8e17ef602a69d6870`, both 1,027 files. This [source-drifted cycle](mail-safety-results/resume-e4fc-source-drifted-cycle-2026-10-23.json) is failed evidence, not a clean current-source pass. A fresh cycle uses the latter digest.

[Hosted run 39929067906](https://github.com/DakiaMail/dakia-desktop/actions/runs/39929067906) on `00e00b7` passed frontend and analytics, then failed one IDLE pacing test: its 45 ms maintenance window did not guarantee two reconciliation cycles on slower hardware. The [oracle correction and sensitivity record](mail-safety-results/ci-idle-noise-oracle-correction-2026-10-23.json) replaces deadline-based repetition with cancellation after the second accepted IDLE, exactly two IDLE/DONE pairs on one connection, and a measured 100 ms pacing interval. A test-only zero-delay perturbation fails. Production pacing is unchanged. Downstream hosted checks were skipped, so this run cannot establish hosted time/cost acceptance.

Native schema and restart driver drafts failed independent review and were withdrawn to `/private/tmp/dakia-native-infrastructure-withdrawn-20261023/`. Their identity, launch and evidence problems are retained as failures. The retry-logging proxy extension was also withdrawn after literal-completion and attribution defects. They do not count as finished infrastructure. The reusable Reader tools and resource collector remain, alongside the reviewed 30k and Gmail lanes. Native upgrade, fatal recovery and interrupted-save checks need fresh actual runtime observations through the existing approved harness.

## Clean source and deep checkpoint

The unchanged `670652ff0da3121eabc3f2ede32d2089828588da85904de8e17ef602a69d6870` / 1,027-file source passed [all 18 source gates](mail-safety-results/resumed-6706-source-2026-10-23.json). A [separate deep run](mail-safety-results/resumed-6706-deep-2026-10-23.json) passed fresh GreenMail and Dovecot integration plus both bounded fuzz targets without a crash artifact. These precede the helper-oracle corrections below and do not establish native acceptance.

The first resumed [live Gmail attempt](mail-safety-results/resumed-gmail-list-parser-failure-2026-10-23.json) failed in the wrapper before selecting or fetching a mailbox. A bounded read-only diagnostic confirmed TLS, login and LIST worked; Python's `imaplib.list()` returns untagged payloads without the `* LIST ` prefix the wrapper wrongly required. A faithful fictional LIST fixture and full fake-client transcript now exercise read-only selections, bounded searches, `BODY.PEEK`, invariant flags and duplicate-mailbox rejection. The failed attempt is retained; a corrected source-bound live proof is still required.

The first governed [30k runner attempt](mail-safety-results/resumed-30k-oracle-and-provenance-failure-2026-10-23.json) passed its production Rust connector/sync/Store/Reader/reopen stage but failed verification. Its CLI oracle incorrectly expected Reader cache content in catalogue search metadata. Its audit omitted valid pre-authentication capability commands. Adding documentation during that run also changed its separately bound Git dirty provenance, although governed source bytes stayed unchanged. Corrected boundary tests must use the actual fictional CLI response and command transcript. This failed runner is not a 30k acceptance pass.
