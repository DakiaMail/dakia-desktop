# Progressive sync native acceptance

The walkthrough uses one fictional account, `reader@example.test`, and local
implicit-TLS IMAP/SMTP fixtures. No real mailbox was connected or modified.

## Runtime identity

The application was built from this checkout using `npm run dev` with an
isolated target directory and a temporary Tauri configuration. Because the raw
development executable was not addressable by Computer Use, the exact compiled
binary was copied into a uniquely identified, ad-hoc-signed test wrapper.

- Wrapper identifier: `dev.dakia.progressive.acceptance.wrapper`
- Executable: `/private/tmp/dakia-progressive-acceptance/Dakia Progressive Acceptance.app/Contents/MacOS/dakia-desktop`
- Main webview, verified through native accessibility: `127.0.0.1:1420/`
- Separate database: `/private/tmp/dakia-progressive-acceptance/data/dakia.db`
- Final fictional server ports: IMAP `59290`, SMTP `59291`
- Fixture conditions: 2,000 sparse Inbox messages, 60 messages in each other
  supported folder, 25 ms response delay, and 20 seconds of history-search delay.

The installed `/Applications/Dakia.app` was not targeted or stopped.

## Confirmed interactions

Account connection displayed messages in Unsorted while the interface still
reported that older mail was downloading. Compose was available during sync.
The first React-row metric was 197 ms, but the initial optional statistics
dialog briefly covered the main window. That value is a DOM-publication
measurement, not an unobstructed first-visible-message timing claim.

Opening a message changed it from Unsorted to Seen, kept the reader open, and
displayed its complete fictional body. The selected message appeared once.
Another message was opened and read while manual sync displayed its progress.
Native screenshots confirmed the subject, sender, body, and reply controls.

The process was stopped at a durable incomplete-sync checkpoint and reopened
with the same database:

| State                      | Before restart    | After restart       |
| -------------------------- | ----------------- | ------------------- |
| Run revision               | 54                | 59                  |
| Stage and outcome          | Deferred, running | Complete, completed |
| Inbox rows                 | 2,000             | 2,000               |
| Sent, Archive, Drafts rows | 60 each           | 60 each             |
| Spam and Trash rows        | 50 each           | 60 each             |

Opening Trash after restart displayed the retained and newly completed folder
contents. Reopening did not clear the existing Inbox catalogue.

## Regressions found by the walkthrough

The real rich-text composer initially produced a prepared MIME artifact without
a Message-ID. SMTP acceptance remained durable and did not trigger a resend,
but Sent reconciliation correctly refused an unidentified copy. Preparation now
creates the Message-ID before serialization. A production-path regression checks
that the persisted artifact, SMTP data, and APPEND use the same ID and bytes.

The selected Unsorted row was initially retained in both Unsorted and Seen.
The component regression now verifies one visible row, preserved selection, and
an open reader after marking it read.

The reader article's entrance animation left all content transparent in the
native test. Removing only that animation immediately made the existing subject,
sender, body, and reply controls visible. Its unused keyframes were removed too.

The fixture itself was corrected to flush delayed QUIT/LOGOUT replies, return
literal-free FLAGS responses, serve body sections, and recognize the exact quoted
Message-ID search sent by production code. Tests exercise these protocol shapes.

## Scope limits

Automatic approval review rejected adding a second test account as outside the
authorized single-mailbox walkthrough. No second account was created. Multiple
account isolation is covered by unit and integration tests instead.

## Automatic Sent publication

The final native executable ran as PID 87000, with the wrapper path and live
webview URL verified again. From the visible composer, the fictional message
`Native final Sent verification` was submitted to `recipient@example.test`.
The composer displayed `Sent, saving copy…` after acceptance and then closed.

SQLite recorded SMTP acceptance 171.962 ms after the durable operation was
created. The submission queue wait was 3.371 ms. The operation completed as
`sent_copy_saved` 367.712 ms after creation, with no error. Its two claims
represent submission and Sent-copy work, not two SMTP submissions.

Before the Sent folder was opened, a read-only database query already found
exactly one row for the subject, at Sent UID 181 with the prepared Message-ID.
Opening Sent through the sidebar displayed that copy without a manual refresh.
This verifies automatic local publication after the remote copy is saved.

The original server transcript records exactly one SMTP acceptance and one
APPEND, with the prepared Message-ID and Sent UID 181. Transcript byte counters
include protocol framing and are not a byte-identity assertion. The core
regression separately verifies exact prepared, SMTP, and APPEND bytes.

Opening the rich-text copy exposed an incorrect fixture BODYSTRUCTURE: it called
a multipart message plain text and returned boundary syntax as its text section.
The fixture now describes and serves the actual mixed/alternative MIME parts.
Its regression checks the same quoted-printable nesting as the native artifact.

For the final reader check, the corrected fixture was restarted and the exact
previous artifact was restored by a setup APPEND at the same UID. That setup
operation is not counted as native send evidence. The unchanged app binary was
reopened as PID 88806. Native accessibility and a screenshot confirmed the exact
subject, sender, recipient, complete body, and reply controls. SQLite recorded
complete cached content. No additional SMTP send was needed.

The [compact native record](evidence/progressive-sync/native-2026-09-12.json)
retains executable identity, durable outcome, protocol counts, and scope limits.
Performance measurements and their exact fixture conditions are in
[the acceptance evidence](initial-sync-acceptance-evidence.md).

## Cleanup

The isolated app, fixture servers, and this task's Vite server were stopped.
The temporary app wrapper, separate mailbox data, and isolated build directory
were removed. Compact evidence remains in this repository; detailed fictional
protocol logs remain under `/private/tmp/dakia-progressive-acceptance/`.
The installed Dakia app and the other worktree's development server were left running.
