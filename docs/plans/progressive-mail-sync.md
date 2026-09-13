This combined plan replaces the earlier versions. It includes faster initial indexing, immediate message visibility, deferred Trash and hidden folders, sending during sync, and the correctness fixes identified in the other clients.

The intended experience is:

1. The account appears after authentication, and Compose becomes available.
2. Dakia displays the first available Inbox headers immediately, targeting an initial batch of 50.
3. Older Inbox mail, Sent, Archive, and Drafts continue syncing in the background.
4. Spam, Trash, and noncanonical special folders are indexed later.
5. Opening a message or sending an email receives priority throughout synchronization.
6. Restarting Dakia resumes completed work without clearing visible mail.

### 1. Establish measurable acceptance criteria

Add instrumentation before changing synchronization so we can compare the same fixture before and after implementation.

Measure authentication-to-first-visible-message time, command counts, downloaded bytes, parser memory, database transaction duration, UI publication delay, and send queue wait.

Use scripted IMAP/SMTP fixtures with approximately 2,000, 30,000, and 100,000 messages. Include dense and sparse UIDs, large headers, overlapping Gmail labels, and injected network delays.

Acceptance requires:

- Initial header fetching uses batch requests, without one request per message.
- Inbox publication does not wait for other folders, previews, classification, or a complete UID inventory.
- Header fetch commands scale approximately with message count divided by batch size.
- Parser buffering remains bounded independently of mailbox size.
- SMTP submission completes while historical IMAP work is deliberately stalled.
- Newly committed rows become visible before the synchronization job finishes.

Record timing targets against the fixture and simulated network conditions. Avoid promising a fixed live Gmail completion time before measuring it.

### 2. Give the backend ownership of account synchronization

Persist the initial synchronization job when the account is created. The native backend starts and resumes it independently of the frontend.

Remove the current sequence where account connection starts realtime monitoring, the frontend starts a full rebuild, and the rebuild stops realtime again.

Track authentication, Inbox availability, historical coverage, and content availability separately. A failed background sync must leave the account and already downloaded messages usable.

Use this folder schedule:

| Stage | Work |
|---|---|
| Initial Inbox | Fetch and publish up to 50 recent Inbox headers |
| Primary history | Rotate bounded batches across older Inbox, Sent, and Archive |
| Secondary | Drafts |
| Deferred | Spam, Trash, and noncanonical special folders |

Opening any supported folder promotes its next batch. User activity must not permanently starve deferred work.

“Hidden folders” retains the agreed scope: special/system folders already managed by Dakia. Arbitrary Gmail labels and custom-folder support are a separate feature.

### 3. Define the initial fetch and historical scan precisely

For the first Inbox batch:

- Open Inbox directly using `EXAMINE`.
- Validate UIDVALIDITY and read the server’s mailbox state.
- Request headers for the last available sequence-number window, including UID in every response.
- Associate results by UID, never response order.
- Publish successfully validated headers promptly.
- Handle expunges or disappearing messages by rechecking and filling the window where needed.

Sequence positions are only a temporary discovery mechanism. Persistent identities and cursors use mailbox, UIDVALIDITY, and UID.

The initial window represents recently added mailbox messages. UIDs do not guarantee ordering by the sender’s date. Store `INTERNALDATE` and message date and apply Dakia’s established display ordering.

For historical discovery:

- Capture a fixed upper UID boundary for each scan.
- Stream UID discovery into a temporary SQLite snapshot in bounded transactions.
- Use a strict incremental parser that does not accumulate the complete response or SEARCH line in memory.
- Treat the snapshot as authoritative only after successful tagged completion.
- Fetch missing headers in UID sets, initially capped at 50 messages and also limited by command bytes, response bytes, literals, and parser depth.
- Reduce batch size when necessary. Isolate an oversized or malformed message without blocking healthy messages indefinitely.

A connection closed during an oversized or incomplete response must be discarded before retrying. Do not reuse a connection with unread response data.

Initial publication must happen before historical inventory begins.

### 4. Make reconciliation safe during concurrent activity

A complete snapshot proves membership only within its captured mailbox generation and UID boundary.

Before removing absent local memberships:

- Verify snapshot completion and matching UIDVALIDITY.
- Restrict reconciliation to the snapshot’s upper UID boundary.
- Preserve memberships created or changed by newer local operations.
- Respect pending moves, deletes, appends, and label changes.
- Reconcile membership separately from message content.

For example, a snapshot ending at UID 200 must never remove realtime mail with UID 201.

Separate “membership discovery complete” from “all headers downloaded.” A failed header fetch is retained for retry and does not mean the message is absent.

New arrivals and ordinary flag changes must not restart the entire historical scan. They enter the realtime or mutation reconciliation path. UIDVALIDITY changes invalidate remote identities and require a replacement generation.

### 5. Use durable state without duplicating every message permanently

Extend existing storage where practical, using these responsibilities:

| State | Purpose |
|---|---|
| Sync run | Account, run identity, stage, readiness, overall outcome |
| Folder sync state | Remote folder identity, UIDVALIDITY, upper boundary, cursor, coverage, retry schedule |
| Mailbox membership | Message location, UID, UIDVALIDITY, observed generation, local mutation version |
| Temporary UID snapshot | Exact membership evidence for an active reconciliation |
| Sparse failure records | Failed messages, error class, attempts, next retry |
| Operation journal | Pending user mutations, SMTP submission, and Sent-copy work |
| Replacement staging | New catalogue generation during UIDVALIDITY recovery or explicit reindex |

The temporary UID snapshot is necessary evidence for reconciliation. It is cleaned up after successful completion and is not a permanent duplicate message catalogue.

Commit message metadata, membership changes, search-index effects, cursor advancement, and progress revision atomically. Emit publication events only after commit.

Migrations must be idempotent, preserve existing message identities where possible, and convert legacy rebuild jobs into resumable jobs. Account removal cleans all associated state and prevents late publication.

### 6. Correct Gmail identity and Archive handling

Separate logical message identity from mailbox membership. Use Gmail’s stable message identifier when available, scoped to the account. For other providers, retain mailbox-scoped identity unless there is sufficient evidence to link copies. RFC Message-ID alone is not a reliable universal deduplication key.

Persist All Mail label observations even when a message is excluded from the Archive view.

A skip applies only to the observed label state. When `\Inbox`, `\Sent`, or other relevant labels change, recompute membership. Include periodic bounded label reconciliation so changes made in another client are eventually detected even without CONDSTORE.

Required regression: a message initially present in Inbox and excluded from Archive becomes visible in Archive after Gmail removes its Inbox label.

### 7. Preserve user actions while synchronization runs

Add or extend an ordered operation journal for read status, stars, moves, copies, deletes, labels, drafts, and Sent-copy work.

Each operation records its immutable target, local version, dependencies, retry state, and outcome. Apply optimistic UI changes immediately.

Background fetches must merge metadata without overwriting newer local flags or membership changes. Use version checks for every background writer, including historical sync, realtime, and post-send reconciliation.

Order dependent operations per affected message or mailbox. Unrelated operations should continue when one fails. Permanent failures restore the affected state with clear feedback.

On UIDVALIDITY changes, pending operations using old UIDs must be safely remapped or surfaced as unresolved. They must never be replayed against reused UIDs.

### 8. Keep sending independent and prevent duplicate submission

Remove the historical-sync lock from the SMTP path. Keep per-account send serialization and coordinate OAuth refresh through one shared refresh operation.

Persist the final outgoing message and submission state before contacting SMTP. Represent submission and Sent-copy state separately:

- Queued or submitting.
- Rejected before acceptance.
- Accepted by SMTP.
- Accepted, with Sent copy pending.
- Delivery uncertain.

The composer responds immediately with a sending state. After confirmed SMTP acceptance, it closes and Sent reconciliation continues independently.

For Gmail, reconcile the provider-created Sent copy. For providers requiring APPEND, queue that separately. A Sent-copy failure must produce “Sent, saving copy…” or equivalent feedback, without inviting the user to resend.

After an ambiguous submission, preserve the message and uncertainty. Do not automatically retry it. Ambiguous APPEND also requires reconciliation before another append attempt.

Account removal first blocks new work and cancels background sync, then waits for bounded active submission handling. Avoid holding lifecycle locks across an entire historical job.

### 9. Prioritize reading and bound background work

Use separate SMTP execution and a bounded IMAP connection budget. Reuse existing connections and scheduling infrastructure where practical.

Within IMAP work, prioritize:

1. User-opened messages, explicit attachment requests, and mutations.
2. Realtime changes and Sent reconciliation.
3. User-requested folder refresh.
4. Primary header history.
5. Drafts and deferred folders.
6. Automatic previews and maintenance.

Use command deadlines, cancellation, byte budgets, and scheduling fairness. A message-count limit alone does not bound how long a slow command occupies a connection.

Automatic preview fetching gets a finite recent-message horizon and total byte budget. Older messages remain eligible for on-demand content loading. Classification follows available content and never gates visibility.

Keep complete metadata indexing for supported folders as the target. User-facing retention controls are not required for this change.

### 10. Make progressive visibility reliable

Add an `Unsorted` Smart Inbox section for unread, unstarred messages with no category. Starred messages remain in Starred, and read messages retain the existing Seen behavior.

New headers must remain visible through preview loading, classification, and thread updates.

Publish account- and mailbox-scoped catalogue events with a monotonic revision. Coalesce reloads and ignore stale query responses. On frontend mount or reconnect, recover state from SQLite rather than depending on missed events.

Expose separate status for available mail, remaining history, deferred folders, retries, and content loading. Do not show “fully synced” while header failures remain.

Keep selection and scroll anchored to stable message identity. If later history merges conversations, preserve the open message through stable thread IDs or an explicit identity remap.

Search should clearly indicate incomplete local coverage while historical indexing continues.

### 11. Define retry, restart, and recovery behavior

Persist the error class, attempt count, next retry time, last committed cursor, and whether user action is required.

Use capped backoff with jitter for temporary network errors and provider throttling. Pause for persistent authentication failures. Isolate repeatable malformed-message failures. Store sanitized diagnostics.

Closing the window must not cancel backend work if Dakia remains running. Quitting or sleeping checkpoints completed work; restarting or reconnecting resumes it without an immediate retry storm.

For explicit reindex and UIDVALIDITY recovery, preserve existing cached messages while building replacement state. Old remote locators become unusable immediately after a UIDVALIDITY mismatch. Cached content can remain readable, with affected remote actions disabled until identity is resolved.

Apply replacement state atomically and preserve user changes made during rebuilding.

### 12. Deliver and verify in controlled slices

Implement in this order:

1. Protocol fixtures, instrumentation, and regression tests.
2. Storage migrations, operation outcomes, and concurrency protections.
3. Independent sending and Sent reconciliation.
4. Initial Inbox batch, Smart Inbox visibility, and publication events.
5. Durable primary backfill, realtime merging, and deferred folders.
6. Safe reindex and UIDVALIDITY replacement.
7. Preview budgets, thread stability, performance tuning, and native acceptance.

These are reviewable implementation slices. Release only when the combined behavior passes the safety and usability gates.

Unit and integration tests are the primary verification. They must cover interrupted snapshots, sparse UIDs, expunges, malformed responses, stale workers, concurrent mutations, label transitions, restart, multiple accounts, and SMTP acceptance followed by Sent-copy failure.

Finish with native Tauri verification using a fictional mailbox: connect, watch messages appear, open a message, compose and send through the local SMTP fixture, switch folders, restart during backfill, and confirm deferred work resumes. Preserve Settings and CLI full-sync semantics throughout.