# Dakia provenance for lettre 0.11.22

This directory is derived from the official `lettre-0.11.22.crate` archive in the Cargo registry cache.

- Package: `lettre`
- Version: `0.11.22`
- License: MIT, retained in `LICENSE`
- Source archive SHA-256: `0da65617f6cb926332d039cb578aad56178da86e128db6a1b09f4c94fa5b3349`
- Upstream VCS revision: `9b88c4ff98fd8653cb389a66f885f02efd79ee6b`
- Dakia SMTP phase-timeout patch SHA-256: `5420143c0f7a2ba808230c42f22257aabc097d4aeb5630384a331d0b3feba20c`
- Independent review document SHA-256: `abc84c5d7bb2e95252693a54bd5277cd9796be7f366ba608ca2fddb772044b92`

The Dakia patch changes:

- `Cargo.toml` and `Cargo.toml.orig`, adding Tokio test utilities for deterministic phase tests.
- `src/transport/smtp/client/async_connection.rs`, adding bounded phase APIs, exact SMTP reply validation, typed DATA failure phases, connection poisoning, and regression tests.
- `src/transport/smtp/client/mod.rs`, exporting the new public phase types.

No files from a generated `target` directory, Cargo extraction marker, or locally regenerated standalone lockfile were copied into this vendor directory. `Cargo.lock` is the unchanged file from the official archive.

The patch was applied to a fresh archive and the focused suite passed 24 tests. Dakia uses the Tokio rustls path. Async-std, native-tls, and boring-tls behavior remains upstream.
