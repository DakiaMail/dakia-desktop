# Dakia

Dakia is a privacy-minded desktop mail application for macOS. It combines multiple IMAP/SMTP accounts, local full-text search, downloadable offline email translation, and a scriptable CLI.

## Current architecture

- `crates/dakia-core` — accounts, provider discovery, SQLite/FTS search, mail transport, translation, and optional AI integrations
- `crates/dakia-cli` — terminal mail operations sharing the desktop profile
- `apps/desktop` — React + Mantine user interface
- `apps/desktop/src-tauri` — Tauri desktop process and command boundary

## Developer setup

Requirements: macOS, Rust 1.82+, Node 20+, and Git LFS.

```bash
npm run setup:worktree
npm run dev
```

`setup:worktree` is safe to rerun. It installs the locked JavaScript
dependencies when missing, materializes the shared Git LFS classifier assets,
reuses a verified ONNX Runtime from another Dakia worktree when available, and
prebuilds the native CLI sidecar. Running `npm run dev` directly also ensures
the required dependencies and assets are present.

## Community

See [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request and
[SECURITY.md](SECURITY.md) for private vulnerability reporting. Participation
is governed by [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

Core and CLI checks:

```bash
cargo test --workspace
cargo run -p dakia-cli -- --help
```

Build platform installers:

```bash
npm run build
```

See [docs/architecture.md](docs/architecture.md), [docs/providers.md](docs/providers.md), and [docs/releasing.md](docs/releasing.md).
