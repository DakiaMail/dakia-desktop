# Contributing to Dakia

Thank you for helping improve Dakia.

## Development

Requirements are macOS, Rust, Node.js 22, npm, and Git LFS. Prepare a checkout
with:

```bash
npm run setup:worktree
```

Before opening a pull request, run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
npm run typecheck
npm run format:check
npm test -- --maxWorkers=1
npm run test:release-scripts
npm run test:translation-worker
npm run build:web
```

Keep pull requests focused, explain user-visible behavior, and add tests for
behavior changes. Do not commit real accounts, messages, credentials, signing
material, or release evidence containing private data.

Releases are built, signed, notarized, and published locally by a maintainer.
Pull requests and CI must never require release secrets.

Report vulnerabilities through [SECURITY.md](SECURITY.md), not a public issue.
