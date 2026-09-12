<p align="center">
  <a href="https://dakiamail.com">
    <img src="apps/desktop/public/icon.png" alt="Dakia logo" width="128" height="128">
  </a>
</p>

# Dakia

Dakia is a privacy-minded desktop mail application for macOS, Windows, and Linux. It combines multiple IMAP/SMTP accounts, local full-text search, downloadable offline email translation, and a scriptable CLI.

[Visit the official Dakia website](https://dakiamail.com)

> [!NOTE]
> Dakia is still under rapid development, and bugs are expected.

## Current architecture

- `crates/dakia-core`: accounts, provider discovery, SQLite/FTS search, mail transport, translation, and optional AI integrations
- `crates/dakia-cli`: terminal mail operations sharing the desktop profile
- `apps/desktop`: React + Mantine user interface
- `apps/desktop/src-tauri`: Tauri desktop process and command boundary

## Developer setup

Requirements: Rust 1.82+, Node 20+, Git LFS, and the build tools for your platform.
See the [release workflow](.github/workflows/production-release.yml) for the
macOS, Windows, and Linux build environments.

```bash
npm run setup:worktree
npm run dev
```

`setup:worktree` is safe to rerun. It installs the locked JavaScript
dependencies when missing, prepares the classifier and platform runtime assets,
and prebuilds the native CLI sidecar. Running `npm run dev` directly also ensures
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

See [docs/architecture.md](docs/architecture.md),
[docs/providers.md](docs/providers.md),
[docs/usage-analytics.md](docs/usage-analytics.md),
[docs/testing-strategy.md](docs/testing-strategy.md), and
[docs/releasing.md](docs/releasing.md).
