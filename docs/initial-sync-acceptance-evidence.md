# Progressive mail sync acceptance evidence

This document maps the combined progressive mail sync plan to executable
evidence. A passing test proves only the condition named in its assertion. A
missing, blocked, or skipped check is not a pass.

## Repeatable baseline runner

Run the focused production-path checks and write a machine-readable report:

```sh
node scripts/initial-sync-acceptance.mjs \
  --output /private/tmp/dakia-initial-sync-acceptance.json
```

The report records the commit, platform, Rust versions, exact Cargo command,
fixture conditions, wall time, stdout, stderr, and the limits of each result.
The wall time includes Cargo process startup and any compilation. It is useful
for comparing the same checkout and machine conditions, but it is not an IMAP
latency measurement by itself.

The existing 30,000 and 100,000 message tests exercise complete membership
reconciliation with every message already present in SQLite. They prove bounded
500-message UID inventory pages and command shape. They do not fetch 30,000 or
100,000 headers, measure first publication, inject latency, or sample peak
memory. The runner says this explicitly so those checks cannot be mistaken for
full mailbox benchmarks.

## Coverage matrix

| Plan requirement                               | Current executable evidence                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               | Missing acceptance evidence                                                              |
| ---------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------- |
| Initial Inbox appears first                    | `initial_inbox_publishes_a_batched_uid_associated_window_before_inventory` checks `EXAMINE`, a recent sequence window, UID association, committed rows, and publication before return. The live runner measures authentication-to-first-commit. The [native walkthrough](progressive-sync-native-acceptance.md) recorded a 197 ms React-row publication while older mail continued downloading.                                                                                                                           | Unobstructed first-visible-row timing because an optional dialog covered the main window |
| Fetch requests are batched                     | Complete dense 2,000 and sparse 30,000 and 100,000 live scans record every production header FETCH. Unit tests cover count and encoded-byte splitting and oversized parser budgets.                                                                                                                                                                                                                                                                                                                                       | Live oversized single-header isolation                                                   |
| Command count scales with batch size           | The live scans record 79, 1,199, and 3,999 header FETCH commands against count-only floors of 40, 600, and 2,000. Every final catalogue has the requested row count.                                                                                                                                                                                                                                                                                                                                                      | Provider-specific command limits that are lower than the fixture's limits                |
| Parser memory is bounded                       | Parser budget tests reject oversized input. Production `UID SEARCH` metrics report an actual maximum retained page of 1,000 UIDs, or 4,000 bytes, for all three live sizes.                                                                                                                                                                                                                                                                                                                                               | Allocator-level peak memory for the complete parser and SQLite process                   |
| Committed rows publish before the job finishes | Initial-window and partial-error tests prove committed production rows before return; React tests publish timing only after a matching row commit. The native row appeared while the historical download remained active.                                                                                                                                                                                                                                                                                                 | Unobstructed first-visible-row timing while historical IMAP is blocked                   |
| SMTP continues during stalled history          | The live production client receives final SMTP acceptance while historical `UID SEARCH` is delayed for five seconds. Native Compose remained usable during sync and showed the accepted and saving-copy state before closing.                                                                                                                                                                                                                                                                                             | None within the single-account fictional walkthrough                                     |
| Restart resumes without clearing visible mail  | Rebuild intent, sync-run revisions, staged replacement outcomes, body cache, and mailbox state have persistence and reopen tests. The native process was stopped at revision 54 and reopened to revision 59 without clearing 2,000 Inbox rows.                                                                                                                                                                                                                                                                            | Repeated process termination at several different durable cursors                        |
| Snapshot reconciliation is safe                | Upper-boundary, local-mutation fence, receipt-conflict, incomplete, stable-empty, CONDSTORE, and UIDVALIDITY tests cover the destructive boundaries.                                                                                                                                                                                                                                                                                                                                                                      | One provider-to-second-Store interleaving that combines every mutation type              |
| Accounts stay isolated                         | The same provider UID is isolated across accounts; concurrent Store, cancellation, late-account-removal, manager-wide budget, and revision tests cover active-account races.                                                                                                                                                                                                                                                                                                                                              | Native two-account cancellation during simultaneous backfill                             |
| Gmail Archive changes follow labels            | Initial and periodic Gmail tests cover Inbox-label removal, sparse UID holes, and older Archive transitions.                                                                                                                                                                                                                                                                                                                                                                                                              | Native Gmail-shaped folder visibility after the catalogue event reaches React            |
| User operations survive background writers     | Durable journal claims, receipt-fenced provider writes, UIDVALIDITY guards, optimistic mailbox reconciliation and rollback, and React optimistic mutation tests cover the main conflict boundaries.                                                                                                                                                                                                                                                                                                                       | Provider restart integration for every operation type and unresolved UI outcome          |
| SMTP outcome and Sent copy are separate        | Prepared bytes, atomic claims, interrupted recovery, APPEND certainty, Message-ID reconciliation, Compose outcome tests, and the stalled-history live scenario cover accepted, pending, and uncertain states. The native operation reached `sent_copy_saved`, and its Sent row appeared automatically before that folder was opened. After the corrected fixture reconstructed the same provider UID and Message-ID, the native reader displayed the exact rich message body and SQLite recorded complete cached content. | None within the single-account fictional walkthrough                                     |
| Scheduling prioritizes user work fairly        | Connection-budget and realtime tests cover reserved higher lanes, failed-UID fairness, maintenance turns, stalled warm batches, cancellation, and manager-wide limits. A native message opened while manual sync continued.                                                                                                                                                                                                                                                                                               | Native mutation preemption timing during a long fixture backfill                         |
| Progressive UI remains stable                  | React tests cover Unsorted visibility, stale revisions, committed timing, incomplete-search status, selection after thread merge, optimistic mutations, and distinct SMTP outcomes. Native checks covered one-row selection, reader visibility, sync, and process restart.                                                                                                                                                                                                                                                | Scroll-position verification across restart                                              |
| Deferred folders follow the declared stages    | Scheduler metadata, persisted sync runs, and backend staged scheduling cover durable folder names, ordering inputs, and revision publication. The native restart completed Sent, Archive, Drafts, Spam, and Trash while retaining all Inbox rows.                                                                                                                                                                                                                                                                         | Repeated opened-folder promotion under sustained new arrivals                            |

## Native fictional mailbox route

The fixture server is local, deterministic, and uses only `.test` addresses. It
supports implicit TLS IMAP and SMTP, 2,000 to 100,000 generated messages, sparse
UIDs, configurable per-response delay, SMTP submission, and retained IMAP
APPEND artifacts. Every selected mailbox has bounded membership and independent
flags. `UID STORE` changes remain visible through later sequence and UID FETCH
commands, which lets the native check prove that opening a message does not
bounce back to unread. It prints the selected ports once, and prints a redacted
protocol event log when it stops.

Start it with a fresh CA output path:

```sh
node scripts/initial-sync-fixture-server.mjs \
  --messages 2000 \
  --folder-messages 25 \
  --delay-ms 25 \
  --sparse \
  --write-ca /private/tmp/dakia-initial-sync-fixture-ca.der
```

Add `--imap-port <port> --smtp-port <port>` when a restart test must keep the
account's saved endpoints. Omitting them lets the operating system select free
ports.

Native verification needs two debug-only seams:

1. `DAKIA_ACCEPTANCE_DATA_DIR` selects a fresh absolute directory dedicated to
   this run. This prevents the fixture account, credentials, mail, and WebKit
   state from touching the normal `dev.dakia.mail` data.
2. `DAKIA_ACCEPTANCE_FIXTURE_CA_DER` adds the generated fixture CA only for
   `127.0.0.1` or `localhost` IMAP and SMTP endpoints in debug builds. Invalid
   input must fail closed. Release builds and non-loopback hosts must ignore or
   reject this opt-in.

Use a temporary Tauri config with a unique identifier and window title, the
normal `http://127.0.0.1:1420/` dev URL, and an isolated target directory. Do
not set `VITE_DAKIA_DEMO_API=1`; the native test must exercise Tauri, SQLite,
IMAP, and SMTP. Through the visible Custom account form, enter
`reader@example.test`, the printed loopback ports, TLS for both transports, and
any fictional password. The fixture accepts the password but never writes it to
its event log.

For example, create `/private/tmp/dakia-initial-sync-tauri.json` with a unique
identifier for this acceptance run:

```json
{
  "identifier": "dev.dakia.acceptance.initialsync.20260912",
  "app": {
    "windows": [
      {
        "label": "main",
        "title": "Dakia Initial Sync Acceptance 20260912",
        "width": 1440,
        "height": 900,
        "minWidth": 960,
        "minHeight": 640,
        "decorations": true,
        "titleBarStyle": "Overlay",
        "hiddenTitle": true,
        "trafficLightPosition": { "x": 14, "y": 18 },
        "transparent": false
      }
    ]
  },
  "bundle": { "active": false }
}
```

After the fixture server prints its ports and writes the CA, start the current
checkout from another terminal. `scripts/dev.sh` forwards the additional Tauri
arguments, so the temporary config is merged with the normal development
config:

```sh
mkdir -p /private/tmp/dakia-initial-sync-app-data
DAKIA_ACCEPTANCE_DATA_DIR=/private/tmp/dakia-initial-sync-app-data \
DAKIA_ACCEPTANCE_FIXTURE_CA_DER=/private/tmp/dakia-initial-sync-fixture-ca.der \
CARGO_TARGET_DIR=/private/tmp/dakia-initial-sync-native-target \
npm run dev -- --config /private/tmp/dakia-initial-sync-tauri.json
```

The config identifier and window title must be changed for a later run. The
data and target paths must be dedicated children of `/private/tmp`, not the
directory itself. The backend rejects a broad or relative acceptance data
path. Keep the fixture process running for the complete restart test so the
same generated mailbox and port values remain available.

Follow `docs/tauri-dev-ui-verification.md` to prove the temporary executable
path and live webview URL before treating UI behavior as evidence. Then connect,
observe the first Inbox batch, open a message, compose and submit to another
`.test` address, change folders, return to Inbox and confirm that the opened
message remains read, stop during backfill, and relaunch with the same isolated
data directory. Confirm that visible mail remains, the saved cursor continues,
and the submitted artifact appears in Sent without matching any Inbox
Message-ID. Stop the fixture and remove only the explicitly created temporary
wrapper and data directory after collecting evidence.

## Baseline status

The committed pre-change source was exported from commit
`8af5c397d81cc9aa60ddaca9b5eb15ecac9f6255` into an isolated directory. The
tests used Rust and Cargo 1.89.0 on Darwin 25.6.0 arm64. They shared the existing
target cache, and the table reports only runs whose Cargo output said the test
profile was already finished before execution.

| Committed fixture                                |    Asserted requests | Rust test time | Whole command time | Maximum resident set size reported by `/usr/bin/time -l` |
| ------------------------------------------------ | -------------------: | -------------: | -----------------: | -------------------------------------------------------: |
| 30,000 messages, dense UIDs, all rows preseeded  |  60 membership pages |         8.36 s |             8.69 s |                                        146,046,976 bytes |
| 100,000 messages, dense UIDs, all rows preseeded | 200 membership pages |        29.53 s |            29.88 s |                                        403,619,840 bytes |

These are real measurements, but the fixture constructs and inserts a complete
`Vec<MailSummary>` before protocol reconciliation. Its memory grows with the
fixture catalogue. The reported resident set covers the complete test process,
SQLite seeding, protocol fixture, and assertions. It does not isolate parser
retention and cannot prove that the parser is bounded independently of mailbox
size. The nearly linear test time likewise measures local setup and
reconciliation without network delay, not Gmail sync time.

The committed fresh-catalogue fixture has only three messages. It proves one
metadata UID-set request and one preview UID-set request, but it is too small to
serve as the requested 2,000, 30,000, or 100,000 fresh-mail baseline. The
production-path measurements below therefore establish the fresh-mail behavior
directly; they are not presented as a before-and-after speedup against the
preseeded baseline.

## Current production-path measurements

The live runner used an empty SQLite store for each size and the public
`MailService` against an implicit-TLS loopback server. The 2,000-message case
used dense UIDs; the larger cases used deterministic sparse UIDs. It committed
a 50-message initial Inbox window, streamed the full UID inventory into
1,000-UID durable pages, and repeatedly called the bounded historical entry
point until every header was committed and one final call returned no work.
The runner builds the Rust example once, runs every scenario from that same
executable, and records its SHA-256 in the report. A source edit during a long
measurement cannot silently change the binary between mailbox sizes.

Run it again with:

```sh
node scripts/progressive-sync-live-acceptance.mjs \
  --output /private/tmp/dakia-progressive-live-acceptance.json \
  --sizes 2000,30000,100000 \
  --history-delay-ms 5000
```

Measured on 2026-09-12 on Darwin arm64 with zero injected response delay for
the fresh-window runs. All rows below came from executable SHA-256
`7696aed680f8bfa9b5116006623042fde2f5a4a31c713b094d524f3f9301ace9`:

| Fresh mailbox       | Authentication to first 50-row commit | Complete header backfill | Header commit batches | Measured publication transactions | Mean publication transaction | Maximum publication transaction | Header FETCH commands | `ceil(N/50)` floor |
| ------------------- | ------------------------------------: | -----------------------: | --------------------: | --------------------------------: | ---------------------------: | ------------------------------: | --------------------: | -----------------: |
| 2,000 dense UIDs    |                              63.06 ms |                  3.019 s |                    40 |                                45 |                     18.48 ms |                        30.23 ms |                    79 |                 40 |
| 30,000 sparse UIDs  |                              60.74 ms |                 95.664 s |                   600 |                               633 |                     22.44 ms |                       129.95 ms |                 1,199 |                600 |
| 100,000 sparse UIDs |                              60.07 ms |                703.783 s |                 2,000 |                             2,103 |                     22.67 ms |                       210.63 ms |                 3,999 |              2,000 |

| Fresh mailbox       | UID pages | Maximum retained UID page storage | IMAP commands | IMAP response bytes | Largest fixture response write |
| ------------------- | --------: | --------------------------------: | ------------: | ------------------: | -----------------------------: |
| 2,000 dense UIDs    |         2 |                       4,000 bytes |           243 |       734,967 bytes |                   32,418 bytes |
| 30,000 sparse UIDs  |        30 |                       4,000 bytes |         3,603 |    11,108,762 bytes |                  176,327 bytes |
| 100,000 sparse UIDs |       100 |                       4,000 bytes |        12,003 |    37,479,769 bytes |                  662,994 bytes |

Every final catalogue contained exactly the requested number of rows. The
header FETCH count is above the count-only `ceil(N/50)` floor because the
production encoder also enforces its encoded command-byte limit. In these
fixtures, almost every 50-UID commit required two FETCH commands. The measured
count proves that both limits were active instead of treating 50 as the only
batch constraint. The measured publication count includes every successful
instrumented SQLite publication transaction, so it is larger than the number
of header commit batches.

The retained-byte metric is the actual maximum `Vec<u32>` page storage in the
production streaming parser: 1,000 UIDs times four bytes. It excludes Tokio
socket buffers, allocator overhead, SQLite, and the test server. The fixture
writes the complete SEARCH response in one operation, which explains the
growing last column while the production parser's retained UID page remains
fixed.

The concurrency scenario delayed only the historical `UID SEARCH` response by
5,000 ms. Production SMTP received its final acceptance in 4.03 ms while the
historical task was still running; history completed in 5,163.37 ms and then
committed its first 50 headers. The durable submission waited 104.48 ms before
its first claim, measured as one queue event. The fixture recorded five SMTP
commands, seven responses, 234 response bytes, and one accepted fictional
message. This proves transport independence and the queue timer in the Rust
production path. Native Compose behavior remains a separate acceptance layer.

The final machine-readable evidence is the checked-in
[live benchmark report](evidence/progressive-sync/live-benchmark-2026-09-12.json).

The definitive serialized core suite passed 429 tests with zero failures and
zero ignored tests in 77.15 seconds. The current acceptance-script suite
passes 18 tests with zero failures and zero
skips:

```sh
node --test \
  scripts/initial-sync-acceptance.node-test.mjs \
  scripts/initial-sync-fixture-server.node-test.mjs \
  scripts/progressive-sync-live-acceptance.node-test.mjs
```

The focused production-path runner also passed all seven declared cases. Its
machine-readable evidence is the checked-in
[focused regression report](evidence/progressive-sync/focused-regressions-2026-09-12.json).

## Final integrated verification

The final product sources passed all four complete suites, with no failures or
ignored tests:

| Suite                   | Passed |
| ----------------------- | -----: |
| Core library            |    429 |
| Desktop backend         |    115 |
| Frontend, 40 test files |    389 |
| CLI                     |     19 |

TypeScript type checking, Rust formatting, changed frontend formatting, and
diff whitespace checks also passed. The acceptance scripts passed 18 tests and
the focused production-path runner passed seven cases. Fixture-only MIME
corrections were verified by the acceptance scripts and the final native reader
walkthrough; they did not change the frozen product binary.

The [native walkthrough](progressive-sync-native-acceptance.md) records account
connection, progressive visibility, reading during sync, sending, automatic Sent
publication, and restart from an incomplete deferred-folder checkpoint. It also
states the remaining measurement and multi-account native coverage limits.
