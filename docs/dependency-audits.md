# Dependency audit policy

Dakia's canonical local dependency gate is:

```sh
npm run audit:dependencies
```

It requires `cargo-audit` and refreshes the RustSec advisory database before
scanning `Cargo.lock`. Install the tool with:

```sh
cargo install cargo-audit --locked
```

The gate rejects Rust vulnerabilities at low severity or above, unsound
dependencies, and yanked dependencies. Unmaintained dependencies remain visible
as warnings so they can be planned without making every upstream maintenance
transition an immediate release blocker.

`cargo-audit` scans every package recorded in `Cargo.lock`, including inactive
features and other operating-system subgraphs. `.cargo/audit.toml` therefore has
two narrow exceptions, and `scripts/audit-dependencies.sh` checks both supported
release targets before applying them:

- `RUSTSEC-2023-0071`: `rsa 0.9.10` is present through the inactive
  `sqlx-mysql` feature subgraph. It is absent from both
  `aarch64-apple-darwin` and `x86_64-apple-darwin`.
- `RUSTSEC-2024-0429`: `glib 0.18.5` is present in Tauri's Linux GTK subgraph.
  It is absent from both supported macOS targets.

If either package becomes reachable on a supported target, the script fails
before `cargo-audit` can apply the exception. New vulnerabilities and new
unsound advisories fail normally.

## Refreshed RustSec evidence

On 2026-07-27, `cargo-audit 0.22.2` scanned the locked 730-package graph against
1,169 advisories from RustSec database commit
`29638ff054fdbb83d2844240f7ef7e576cb52629` (2026-07-25).

The unfiltered lockfile scan found one vulnerability,
`RUSTSEC-2023-0071` in `rsa 0.9.10`, and one unsound advisory,
`RUSTSEC-2024-0429` in `glib 0.18.5`. The supported-target `cargo tree` checks
above found neither package reachable on Apple Silicon nor Intel macOS.
The scan also reported 17 unmaintained-package warnings; those are tracked as
warnings under the policy above.

## npm audit

The npm audit endpoint receives dependency metadata. It is intentionally not
contacted by the default local gate. Once that network disclosure is
authorized, run:

```sh
npm run audit:dependencies:npm
```

That command first runs the Rust gate, then audits both `package-lock.json` and
`apps/site/package-lock.json` at the `low` threshold. It does not run dependency
lifecycle scripts or modify either lockfile.

On 2026-07-27, the root lockfile audit checked 262 dependencies and reported
zero vulnerabilities. The site lockfile initially reported three high-severity
findings through the development-only chain
`wrangler -> miniflare -> sharp 0.34.5`. Updating Wrangler from 4.112.0 to
4.114.0 resolved Miniflare to 4.20260722.0 and Sharp to 0.35.2. npm's
post-install audit then checked the remediated 110-package site graph and
reported zero vulnerabilities. A separate explicit lockfile audit checked 185
dependencies and likewise reported zero vulnerabilities. The site TypeScript
and Vite production build also passed after the update.
