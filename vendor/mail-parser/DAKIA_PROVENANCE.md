# Vendored upstream provenance

This directory is a copy of `mail-parser` 0.11.5 from crates.io.

- Registry checksum: `47785d444be4d32c1709171c6219a90f667c0ad0ffe68b4b179e794f31f4f9e8`
- Upstream repository: `https://github.com/stalwartlabs/mail-parser`
- Published source revision: `67a53e3ded665d68d9fc66cb41d89b3ded559d0e`
- License: Apache-2.0 OR MIT. The upstream license files and copyright headers
  are retained unchanged.

## Dakia changes

The parser documented `MessageParser::parse` as non-panicking, but malformed
nested `message/rfc822` input can reach recovery branches whose part identifier
is absent. Upstream issue [#160](https://github.com/stalwartlabs/mail-parser/issues/160)
confirms this is reachable through fuzzing, including in 0.11.9.

Each affected recovery branch now returns `None` rather than reaching an
internal `debug_assert!` or retaining a structurally inconsistent partial
message. Dakia converts that `None` into its ordinary MIME parse error.

The receive parser also applies narrow RFC 5322 obsolete-input compatibility
rules required at Dakia's message boundary: three-digit years use the RFC's
1900 offset, direct source-route components are removed from the semantic
mailbox identity, and CFWS inside obsolete message identifiers is not retained
as identifier data. `resources/address.json` changes the corresponding
upstream direct-route expectation from the transport route to its terminal
mailbox, and `resources/id.json` changes the obsolete-CFWS expectation to the
canonical identifier. These receive-only changes do not alter outbound message
generation.
