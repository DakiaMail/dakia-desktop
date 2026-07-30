# Testing and Fixture Strategy

Status: implementation plan

Baseline reviewed: `origin/main` at `0a3182e` on 2026-07-30

This document defines how Dakia should prevent repeats of production issues
without returning to slow, expensive GitHub Actions. It is a plan, not a claim
that the described harnesses, workflows, or gates already exist.

## Goals

- Catch failures at the user-visible and durable production boundaries rather
  than only testing parser helpers or mocked components.
- Turn every confirmed production defect into a faithful, redacted regression
  fixture and a test that fails on the broken implementation.
- Keep automatic pull-request validation fast and inexpensive.
- Keep signing, notarization, packaging, publication, live-provider checks, and
  native Apple-Silicon acceptance outside ordinary pull-request CI.
- Report skipped, unavailable, or environment-blocked validation accurately;
  none of those states counts as a pass.

## Current baseline and lessons

Latest `main` already contains a substantial regression foundation:

- 13 checked-in raw `.eml` fixtures and four realistic HTML fixtures;
- MIME limits for raw message size, aggregate headers, part count, and nesting;
- complete, catalogue, and header parse-path tests;
- storage/restart, attachment-presentation, body-cache, fetch-claim, realtime,
  Reader, and HTML disclosure tests;
- typed message-content failures that distinguish retryable failures from
  deterministic MIME resource-limit failures.

The recent issue-driven changes show the remaining systemic weaknesses:

- Provider-specific MIME can behave differently in the complete parser and the
  selective IMAP `BODYSTRUCTURE`/section-fetch path.
- Correct parser output can still diverge from persisted data, Tauri
  serialization, frontend decoding, Reader state, or WebKit rendering.
- Cache, refresh, reply provenance, flags, moves, deletion, and account removal
  have important asynchronous orderings that ordinary happy-path tests miss.
- Current IMAP/SMTP tests mostly construct commands and responses; they do not
  exercise real socket framing, TLS/STARTTLS, cancellation, reconnect, delivery
  uncertainty, or the complete provider-to-UI event path.
- The comprehensive local verification script is not an automatic merge gate.
  A test that is never run cannot prevent a regression.

The old plan item "create a MIME corpus" is therefore complete enough to be
replaced by corpus governance and cross-path conformance. The highest-value new
work is efficient CI enforcement, fixture-path parity, scripted protocols,
state-machine testing, and bidirectional Tauri contracts.

## Regression contract for production defects

A production defect is not complete until its fix includes all applicable
items below:

1. Capture the smallest faithful redacted input. Preserve provider nesting,
   attributes, whitespace, sibling order, line endings, malformed structures,
   and other details that contributed to the failure.
2. Exercise the production selection and event path. Do not stop at a private
   helper if the failure can occur during candidate discovery, persistence,
   serialization, event delivery, rendering, or state cleanup.
3. Assert the user-visible or durable result: visible content, disclosure
   state, attachment presentation, stored rows, emitted event, retry behavior,
   or remote command outcome.
4. Add adjacent adversarial variants: empty, malformed, nested, adjacent,
   ambiguous, multiple-candidate, and must-remain-untouched cases.
5. Add restart and deterministic concurrency coverage when stale work can
   outlive the request or mutate durable state.
6. Prove sensitivity for the actual defect by running the regression against
   the broken revision or a representative boundary mutation.
7. Record the fixture and focused verification command in the pull request.
8. Include the test in an automatic or explicitly required local gate. A
   skipped or unexecuted test is not evidence.

## Test architecture

### Fixture manifest and governance

Create one machine-readable manifest covering realistic `.eml` and HTML
fixtures. Each entry contains:

- a stable ID and repository path;
- a checksum when exact malformed bytes matter;
- synthetic or faithfully redacted provenance;
- the issue or regression it protects;
- provider shape without claiming live-provider compatibility;
- expected body, HTML, snippet, attachment, error, and resource-limit semantics;
- applicable publication/fetch paths;
- redaction reviewer, review date, and permitted test domains.

The validator must fail for unmanifested files, missing files, duplicate IDs,
checksum drift, prohibited addresses/domains, credential-like values, or a
manifest entry not exercised by a test. Automated scanning supplements, but
does not replace, human redaction review.

### Multi-path message conformance

Every suitable raw fixture should run through:

1. complete RFC822 parsing;
2. catalogue and header parsing;
3. partial preview;
4. selective IMAP `BODYSTRUCTURE` and section fetching;
5. file-backed SQLite persistence and reopen;
6. the production Tauri `MessageContent` serialization shape;
7. TypeScript decoding and Reader/HtmlMessage semantic assertions.

For paths expected to be equivalent, compare normalized body text, HTML,
snippet, attachment identity/presentation, and error category. Unsupported or
intentionally different paths must declare the expected difference explicitly.
Malformed/resource-limit fixtures must assert stable typed failures rather than
only "does not panic."

### Scripted IMAP and SMTP

Build reusable local TLS and STARTTLS servers with ordered expectations,
dynamic IMAP tags, exact transcripts, fault injection, and short test-specific
deadlines. Drive public `MailService` sync, send, and realtime operations
through temporary SQLite and a test event sink.

The blocking scenario set should include:

- fragmented and coalesced tagged/untagged responses and literals;
- literals associated with the wrong UID or BODY section;
- unsolicited responses, `BYE`, EOF, timeout, and cancellation;
- IDLE renewal/reconnect and mailbox-scoped UIDVALIDITY changes;
- Gmail labels, missing SPECIAL-USE, and unusual namespace/delimiter behavior;
- read-neutral selective fetches and bounded literal/transcript allocation;
- TLS, STARTTLS, and authentication rejection;
- SMTP recipient rejection, timeout before/after DATA, uncertain delivery,
  APPEND failure, COPYUID behavior, and duplicate-Sent prevention;
- Inbox versus Sent event semantics, remote flag/deletion reconciliation,
  restart recovery, and account isolation.

Protocol tests assert both the exact conversation and final SQLite/event state.

### State, persistence, and process boundaries

Use deterministic scheduling or explicit barriers to cover interleavings among
message selection, expand/reply/forward, fetch, cache warming, star/unstar,
read/unread, move, delete, account removal, quiet refresh, and restart. A stale
completion must never resurrect a message, attachment, flag, or cache entry.

Add:

- independent Store connections contending on one database;
- bounded busy handling and injected write failures;
- interruption immediately before and after transactional checkpoints;
- repeated reopen after interrupted migration/rebuild/cache work;
- corrupt-row isolation and whole-database corruption behavior;
- CLI subprocess tests with isolated state, stdout/stderr schemas, exit codes,
  cancellation, restart persistence, and bundled-sidecar parity.

### Tauri command and event contracts

Maintain a mechanical inventory of:

- frontend production invokes;
- Rust registered handlers;
- native emitted events;
- frontend and compose-window listeners.

Fail when the sets diverge. Shared payload fixtures must be decoded by both
Rust and TypeScript. Test exact names, top-level argument keys, camel/snake
casing, UUID/cursor shapes, missing/null fields, channels, cleanup, remounts,
late events, and sanitized error envelopes. Demo-only APIs are excluded from
the production inventory explicitly.

### Property, fuzz, and native layers

- Run fixed-seed property tests in the ordinary source suite for multipart
  trees, MIME parameters, IMAP framing, CID rewriting, quoted-history markers,
  filenames, thread graphs, and state interleavings.
- Run longer fuzzing only manually or in a separately approved schedule. Every
  discovered failure becomes a checked-in blocking seed or fixture.
- Use representative mutation/sensitivity evidence for high-risk boundaries;
  do not require an expensive mutation run for every generated case.
- Use native Apple-Silicon WebKit verification only for layout, iframe sizing,
  focus, external links, remote-content policy, and repeated disclosure
  transitions that jsdom cannot prove.

## Cost-effective GitHub Actions

### Hard constraints

- Ordinary pull requests must not use macOS runners.
- Ordinary pull requests must not package the app, build release artifacts,
  sign, notarize, publish, run live-provider tests, download translation
  models, or prepare release-only ONNX/classifier assets.
- Do not run a platform matrix.
- Do not run the same full suite in multiple jobs.
- Cancel superseded runs on the same pull request.
- Target a warm-cache automatic run below 10 Linux billable minutes and an
  estimated hosted-runner cost below USD 0.25 per pull request.
- If the rolling median exceeds the time or cost target, optimize or narrow the
  automatic lane before adding more required work.

### Required automatic lane

Use one change-classification job and at most one Linux test job.

The classifier derives scope from the merge-base diff:

- `docs-only`: documentation/metadata validation only;
- `frontend`: formatting, typecheck, and relevant Vitest suites;
- `rust-core`: formatting/Clippy for changed packages and relevant Rust suites;
- `mail-boundary`: full core mail/storage/protocol fixtures plus Reader/API
  contract suites;
- `tauri-boundary`: Linux-compilable Tauri command/event contract tests;
- `release-only`: release-script tests without building an app.

Shared types, API contracts, fixture manifests, lockfiles, test configuration,
or scope-classifier changes promote the run to the broader applicable scope.
The classifier itself has table-driven tests so path mistakes cannot silently
skip validation.

Workflow efficiency requirements:

- use `concurrency` with `cancel-in-progress: true`;
- use shallow checkout and Git LFS only when the selected scope needs an LFS
  fixture;
- restore npm and Cargo caches keyed by lockfiles, runner, architecture, and
  toolchain;
- use `npm ci --ignore-scripts` where compatible with the selected frontend
  checks;
- do not call `setup:worktree` in automatic CI because it combines dependency,
  LFS, ONNX, and CLI preparation that most scopes do not need;
- keep build/test output concise and upload reports only on failure or when
  needed for the coverage baseline;
- pin action revisions and toolchain versions.

The single required status reports each applicable scope as passed, skipped due
to an inapplicable diff, or failed. A skip caused by missing infrastructure or
an environment error must fail the required status rather than appear green.

### Local and manually dispatched lanes

`npm run verify:local` remains the authoritative full source gate before merge
for high-risk mail, storage, Tauri, native, and release changes. Pull requests
record its result and focused sensitivity evidence.

Manually dispatched GitHub workflows may provide reproducible remote evidence
for:

- the full Linux source suite;
- coverage generation;
- longer fuzzing;
- credentialed provider smoke tests.

They are disabled by default, have explicit job timeouts, and require a manual
reason/input. No scheduled workflow is enabled without separate approval and a
measured cost estimate.

Apple-Silicon native verification and the entire release flow stay local. CI
never receives signing, notarization, OAuth, R2, or production mailbox secrets.

### Coverage ratchet without per-PR duplication

Do not calculate full coverage on every pull request; it recompiles and reruns
most of the suite for little additional defect-detection value.

Instead:

1. Pin Rust and frontend coverage tools and normalize exact covered/uncovered
   line, function, and branch counts per package.
2. Generate candidate baselines locally or with a manual Linux dispatch.
3. Require three repeat green runs before committing a baseline.
4. Compare the baseline during explicitly requested coverage runs and before a
   release, not during every ordinary pull request.
5. Review baseline changes as intentional diffs; never let the workflow under
   test update its own baseline.
6. Exclude generated/demo/fixture code explicitly, but never exclude protocol,
   parser, storage, bridge, Reader, realtime, or CLI production code.

Semantic fixture inventory and contract completeness remain blocking on every
relevant pull request; coverage is a periodic non-regression guard, not a
substitute for those gates.

## Delivery sequence

1. Add the tested change classifier and minimal Linux workflow. Measure three
   representative runs before making it required.
2. Add the fixture manifest, redaction validator, ownership, and automatic
   corpus enumeration.
3. Make existing raw fixtures equivalent across complete, selective, storage,
   Tauri, and Reader paths.
4. Add the scripted IMAP/SMTP harness and provider capability profiles.
5. Enforce bidirectional Tauri command/event contracts.
6. Add deterministic concurrency, locking, restart, and CLI subprocess tests.
7. Add fixed-seed properties, then manual fuzzing and the periodic coverage
   ratchet.
8. Add disabled manual provider-smoke infrastructure. Enabling credentials or
   scheduling it is a separately approved change.
9. Keep native WebKit and installed-upgrade verification in the supervised
   Apple-Silicon acceptance and release process.

## Activation and cost review

Creating workflow files does not automatically authorize:

- enabling branch-protection requirements;
- enabling scheduled workflows;
- adding repository or environment secrets;
- running credentialed provider tests;
- using paid macOS or larger runners.

Each requires separate approval after the workflow's measured duration and
estimated monthly cost are reported. The first implementation should remain
non-required until three representative pull requests demonstrate that the
automatic lane stays within the documented budget.
