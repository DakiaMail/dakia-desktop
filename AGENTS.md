# Repository Agent Guidance

## Subagents

For longer tasks that can be divided into independent, bounded workstreams,
proactively use subagents without waiting for the user to request delegation.
Good candidates include codebase exploration, separate component or risk
reviews, test-gap analysis, running independent test suites, and investigating
unrelated failures or logs.

Use the project-scoped agents under `.codex/agents/` when their role matches:

- `dakia_explorer` for fast, read-only execution-path and dependency mapping.
- `dakia_implementer` for a clearly owned implementation slice.
- `dakia_test_engineer` for adversarial test design and independent test runs.
- `dakia_reviewer` for correctness, security, concurrency, and regression review.
- `dakia_native_release_verifier` for Tauri, macOS, signing, notarization,
  updater, and release verification.

Give each subagent a concrete scope and expected result, run independent
workstreams in parallel, and wait for all relevant results before synthesizing
the outcome. Keep requirements, cross-cutting decisions, integration, and final
verification with the main agent.

Prefer parallel read-heavy work. For write-heavy work, assign non-overlapping
files or components with clear ownership; otherwise keep implementation
sequential to avoid edit conflicts and coordination overhead. Do not use
subagents for small tasks where delegation would add more overhead than value.

## Fresh worktrees

Run `npm run setup:worktree` before development or native checks in a fresh
worktree. The command is idempotent and prepares locked JavaScript dependencies,
Git LFS classifier assets, the macOS ONNX Runtime, and the debug CLI sidecar.
`npm run dev` performs the required subset automatically.

## Testing

Always rely on unit and integration tests as the primary form of verification.
Do UI testing only after exhausting those layers, at the very end.

Write regression tests from the actual failing input and user-visible outcome,
not merely from the current helper implementation. When a bug comes from email
markup, protocol data, database state, or another structured external input:

- Capture the smallest faithful, redacted fixture from the real input. Preserve
  provider-specific nesting, attributes, whitespace, and sibling relationships;
  do not replace it with cleaner invented markup that only exercises the
  expected code path.
- Assert semantics at the component or integration boundary. For rendering
  regressions, verify what remains visible, what moves behind a disclosure, the
  number and state of controls, and the complete expand-collapse-expand round
  trip. A helper return value alone is not sufficient.
- Include adversarial variants: empty and malformed structures, nested and
  adjacent markers, multiple candidates, whitespace and `<br>` differences,
  uncertain lookalikes, and content that must remain untouched.
- Exercise the production selection and event path. Avoid tests that call an
  exported helper directly when the failure can occur in candidate discovery,
  early-return behavior, event wiring, iframe messaging, or state cleanup.
- For iframe or size-dependent behavior, use a controllable `ResizeObserver`,
  mocked measurements, real toggle events, and flushed animation frames. Assert
  both growth and shrinkage. Remember that jsdom has no layout engine, so a
  jsdom-only height assertion is not evidence of WebKit layout behavior.
- Prefer a small checked-in corpus of redacted real provider fixtures over many
  simplified inline strings. Add the fixture that exposed each regression to
  that corpus.

Before declaring a regression fixed, explain which test would have failed on
the broken implementation. Temporarily reverting or otherwise perturbing the
fix is encouraged when practical to prove the test is sensitive to the defect.

## Native macOS verification

Before you do native macOS app verification, think: have you done all that is possible in verifying the new functionality via unit and integration tests?

Before launching or controlling Dakia for native Tauri verification, read
[Verifying the Intended Tauri Dev App on macOS](docs/tauri-dev-ui-verification.md).

Do not target an app only by the display name `Dakia`. Prove the executable
path and the live webview URL before treating UI output as evidence. Use
`npm run dev` for Tauri/backend behavior; `npm run dev:web` is browser demo data
only.

For behavior that depends on WebKit layout, iframe resizing, focus, native
events, or other capabilities jsdom cannot model, native verification is a
required final acceptance layer rather than optional supporting evidence.
Drive the interaction as a user through every affected state transition and
verify the final visible layout, including repeated expand/collapse cycles.
Use an isolated, clearly fictional mailbox fixture whenever possible. If a
specific real message is required to reproduce a provider structure, keep the
test read-only, do not send or modify mail, and convert the redacted structure
into a checked-in regression fixture afterward.

## macOS release publishing

Before building, signing, notarizing, or publishing a new Dakia macOS release,
read [Publishing A macOS Release](docs/publishing-macos-release.md).
