# Vendored upstream provenance

This directory is a copy of `tauri-plugin-updater` 2.10.1 from crates.io.

- Registry checksum: `806d9dac662c2e4594ff03c647a552f2c9bd544e7d0f683ec58f872f952ce4af`
- Upstream repository: `https://github.com/tauri-apps/plugins-workspace`
- Published source revision: `d6a3898001a4bcc659e045f9501498751b77dbe6` (`plugins/updater`)
- License: Apache-2.0 OR MIT. The upstream `LICENSE_APACHE-2.0`, `LICENSE_MIT`,
  `LICENSE.spdx`, and copyright headers are retained unchanged.

## Dakia changes

Only these local changes are made:

1. The Windows-only optional `zip` dependency changes from the vulnerable 4.x
   series to 9.x in both `Cargo.toml` and `Cargo.toml.orig`. The API used by the upstream updater remains
   `ZipArchive::new(...).extract(...)`.
2. That exact extraction call is factored into a small helper compiled for
   Windows and tests. Host tests exercise normal extraction plus traversal,
   escaping symlink, and privileged-mode archive entries.

All other production updater code is retained from upstream 2.10.1.

The upstream nested `Cargo.lock` is intentionally omitted. Dakia's root
lockfile is the only resolved dependency graph used to build and audit this
vendored workspace member.
