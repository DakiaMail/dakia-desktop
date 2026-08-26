# Cross-platform GitHub release plan

## Objective

Provide an explicitly dispatched GitHub release workflow that creates and
publishes Dakia downloads for macOS Apple Silicon, macOS Intel, and Windows
x64. Keep ordinary pull-request verification Linux-only and cost-bounded.

## Non-goals

- No scheduled release builds.
- No macOS or Windows packaging on pull requests.
- No release, tag, publication, credential, billing, or organization-setting
  mutation without a separate explicit authorization.
- Linux remains a verification platform in this scope; it is not a packaged
  release target.

## Decisions recorded

- GitHub Releases are the requested publishing surface for the three requested
  downloads.
- Existing R2/Tauri updater publication remains an independent follow-up until
  its Apple-Silicon-only manifest and publisher can safely support the new
  targets. A GitHub download release must not silently claim automatic updates.
- Ordinary PR checks retain the existing single Ubuntu classifier/validation
  workflow, its cancellation behavior, and its scoped test selection.

## Phase 1 — release contracts and platform prerequisites

**Status:** in progress

- Make CLI sidecar bundling work on Windows x64, including the `.exe` naming
  convention required by Tauri.
- Establish pinned, verified Windows ONNX Runtime packaging and loading.
- Parameterize macOS packaging/verification for `aarch64` and `x86_64` while
  preserving signing, notarization, archive, and checksum checks.
- Add Windows NSIS installer and installed-artifact verification.
- Extend release-script tests from Apple-Silicon-only assumptions to both macOS
  architectures and Windows where host-independent.

**Acceptance:** Each target creates the expected immutable, checksum-covered
artifact; macOS artifacts are signed/notarized; Windows installer and bundled
CLI are verified on Windows.

## Phase 2 — manually dispatched GitHub workflow

**Status:** pending Phase 1

- Add a `workflow_dispatch` release workflow requiring a version, immutable
  source revision, release notes, a non-empty reason, and an explicit publish
  confirmation.
- Use the trusted self-hosted Apple Silicon runner for both macOS artifacts,
  and a trusted Windows runner for the Windows installer.
- Protect the publish job with a GitHub `release` environment and upload only
  an exact asset allowlist with verified checksums.
- Create the GitHub Release as a draft, download and compare its assets, then
  publish it only after verification.

**Acceptance:** No PR or schedule can invoke a release build. A manual,
protected dispatch produces exactly the three documented downloads or fails
without publishing a partial release.

## Phase 3 — documentation and measured cost controls

**Status:** pending Phase 2

- Replace stale nightly/local-only release documentation with the dispatch,
  runner, secret, verification, rollback, and failure-handling procedures.
- Document the Linux-only PR lane and its cost target.
- Record the DakiaMail organization’s actual Actions usage, included quota,
  budget, spending limit, artifact/cache storage, and the cost of three
  representative PR/release runs.

**Acceptance:** An authorized maintainer can perform a release without relying
on unpublished local knowledge, and the budget report distinguishes measured
usage from configuration or estimates.

## Required owner inputs before Phase 2

1. The GitHub `release` environment must hold/reveal access only to trusted
   release jobs, with approval protection enabled.
2. A Windows code-signing policy and certificate source are required. An
   unsigned NSIS installer is not an acceptable substitute without an explicit
   owner waiver.
3. A trusted Windows x64 runner must be designated. GitHub-hosted Windows is
   acceptable for infrequent manual releases, while a self-hosted Windows
   runner avoids Actions-minute charges but must never accept fork PRs.
4. GitHub CLI or equivalent authenticated read access is required to complete
   the organization billing analysis.
