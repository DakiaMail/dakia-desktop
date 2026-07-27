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

Always rely on unit and integration tests as the primary form of verification. Only do UI testing at the very end and as the last resort.

Be thorough when writing tests and attack the solution ruthlessly. Do some research. Think of creative ways of breaking the solution.

## Native macOS verification

Before you do native macOS app verification, think: have you done all that is possible in verifying the new functionality via unit and integration tests?

Before launching or controlling Dakia for native Tauri verification, read
[Verifying the Intended Tauri Dev App on macOS](docs/tauri-dev-ui-verification.md).

Do not target an app only by the display name `Dakia`. Prove the executable
path and the live webview URL before treating UI output as evidence. Use
`npm run dev` for Tauri/backend behavior; `npm run dev:web` is browser demo data
only.

## macOS release publishing

Before building, signing, notarizing, or publishing a new Dakia macOS release,
read [Publishing A macOS Release](docs/publishing-macos-release.md).
