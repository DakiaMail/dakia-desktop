# Dakia supported mail standards profile

Dakia supports an IMAP4rev1 client profile with selected extensions, authenticated
SMTP submission, MIME reading and composition, mailto actions, and verified
one-click unsubscribe. This is not complete conformance certification for any RFC.
IMAP4rev2 is a modern reference for the explicitly selected behaviors below, not
an advertised replacement for the IMAP4rev1 profile.

The machine-readable [clause matrix](../testdata/mail-standards-requirements.json)
is authoritative for each stable requirement ID, official RFC section, applicability,
profile, decision reason, catalogue links, and exact source test/assertion anchors.
The tables below give its human-readable index. They distinguish a source assertion
from provider, native WebKit, packaged-app, upgrade, and release acceptance.

## Evidence rules

- `tested` declares a reviewed source assertion for the stated slice. It does not
  mean the source suite ran on the current revision. Pair it with the unchanged-source
  execution reports in [mail safety evidence](mail-safety-evidence.md).
- `intentionally_tolerated` records tested handling of malformed interoperable
  input or a documented compatibility deviation, such as preferred header width.
  It does not claim strict grammar or recommendation compliance.
- `unsupported` is an actual product profile exclusion with a reason. Missing
  infrastructure or a missing test never becomes an unsupported declaration.
- `gap` means the applicable requirement is unproven. It can be a missing test,
  an unresolved behavior difference, or incomplete evidence. It is never a pass.

Run `node scripts/validate-mail-standards-requirements.mjs` to validate the matrix,
and `node --test scripts/validate-mail-standards-requirements.node-test.mjs` to check
validator sensitivity. The validator checks required clause IDs, official clause
URLs, catalogue references, test declarations, assertion anchors inside those tests,
and linked assertion helpers. JavaScript callback evidence must contain an
executable top-level assertion or await a directly called, proven helper. Parameterized
evidence accepts literal nonempty tables, optionally followed by one `flatMap`
whose callback returns a literal nonempty array. Empty tables, unknown expressions,
spreads, holes and filters are rejected conservatively. The scanner recognizes
ordinary arrow and `function` callbacks; it does not evaluate JavaScript or prove
runtime case counts. A test without its own assertion qualifies only when its
executable body directly calls the named validated assertion-bearing helper. Comments, literal strings, uncalled references, closures and dead conditional
branches cannot supply that delegation. It rejects removal of reviewed gaps. This prevents
stale references and silent scope shrinkage; human review still decides whether an
assertion proves the stated semantics. It does not execute the referenced tests.

The original 69-gap programme and this clause matrix use different units. Several
clauses can refine one broad scenario. Do not add these counts together or report a
source assertion as full closure of a provider/native requirement.

The reviewed profile contains 97 clauses across 14 RFCs: 82 tested entries, 10 intentionally tolerated entries, 5 profile exclusions, and 0 unmapped entries. These finite-profile counts do not establish full conformance or mean that no unproven behavior remains.

## IMAP4rev1

Primary reference: [RFC 3501](https://www.rfc-editor.org/rfc/rfc3501.html).

| Requirement ID and clause                                                                    | Scoped behavior                                                                   | Evidence status |
| -------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------- | --------------- |
| `MSR-3501-tag` [§2.2.1](https://www.rfc-editor.org/rfc/rfc3501.html#section-2.2.1)           | Match tagged command completions.                                                 | `tested`        |
| `MSR-3501-epoch` [§2.3.1.1](https://www.rfc-editor.org/rfc/rfc3501.html#section-2.3.1.1)     | Reject attachment fetch when remote UIDVALIDITY differs from its cached identity. | `tested`        |
| `MSR-3501-sequence` [§2.3.1.2](https://www.rfc-editor.org/rfc/rfc3501.html#section-2.3.1.2)  | Do not treat shifting sequence numbers as durable UIDs.                           | `tested`        |
| `MSR-3501-seen` [§2.3.2](https://www.rfc-editor.org/rfc/rfc3501.html#section-2.3.2)          | Use Seen for explicit read changes.                                               | `tested`        |
| `MSR-3501-literals` [§4.1.2](https://www.rfc-editor.org/rfc/rfc3501.html#section-4.1.2)      | Consume literal octet counts across fragmented reads.                             | `tested`        |
| `MSR-3501-nil` [§4.3](https://www.rfc-editor.org/rfc/rfc3501.html#section-4.3)               | Accept NIL fields in the tested attachment BODYSTRUCTURE shape.                   | `tested`        |
| `MSR-3501-posttls` [§6.2.1](https://www.rfc-editor.org/rfc/rfc3501.html#section-6.2.1)       | Discard pre-TLS capabilities before authentication.                               | `tested`        |
| `MSR-3501-failure` [§6.2.1](https://www.rfc-editor.org/rfc/rfc3501.html#section-6.2.1)       | Do not authenticate after STARTTLS refusal.                                       | `tested`        |
| `MSR-3501-logindisabled` [§6.2.3](https://www.rfc-editor.org/rfc/rfc3501.html#section-6.2.3) | Respect LOGINDISABLED; do not issue LOGIN.                                        | `tested`        |
| `MSR-3501-sasl` [§6.2.2](https://www.rfc-editor.org/rfc/rfc3501.html#section-6.2.2)          | General SASL mechanism negotiation.                                               | `unsupported`   |
| `MSR-3501-utf7` [§5.1.3](https://www.rfc-editor.org/rfc/rfc3501.html#section-5.1.3)          | Decode modified UTF-7 mailbox names.                                              | `tested`        |
| `MSR-3501-list` [§6.3.6](https://www.rfc-editor.org/rfc/rfc3501.html#section-6.3.6)          | Respect LIST hierarchy delimiters and mailbox attributes.                         | `tested`        |
| `MSR-3501-peek` [§6.4.5](https://www.rfc-editor.org/rfc/rfc3501.html#section-6.4.5)          | Preview and raw export must not implicitly set Seen.                              | `tested`        |
| `MSR-3501-section` [§6.4.5](https://www.rfc-editor.org/rfc/rfc3501.html#section-6.4.5)       | Associate FETCH literals with the requested UID and section.                      | `tested`        |
| `MSR-3501-store` [§6.4.6](https://www.rfc-editor.org/rfc/rfc3501.html#section-6.4.6)         | Apply explicit flags to immutable UID targets.                                    | `tested`        |
| `MSR-3501-bye` [§7.1.5](https://www.rfc-editor.org/rfc/rfc3501.html#section-7.1.5)           | Terminate failed sessions on BYE without spinning.                                | `tested`        |
| `MSR-3501-expunge` [§7.4.1](https://www.rfc-editor.org/rfc/rfc3501.html#section-7.4.1)       | Reconcile unsolicited EXPUNGE after IDLE reconnect.                               | `tested`        |
| `MSR-3501-search` [§6.4.4](https://www.rfc-editor.org/rfc/rfc3501.html#section-6.4.4)        | Preserve SEARCH charset and server failure semantics.                             | `tested`        |

## Selected modern IMAP behavior

Primary reference: [RFC 9051](https://www.rfc-editor.org/rfc/rfc9051.html).

| Requirement ID and clause                                                                   | Scoped behavior                                            | Evidence status |
| ------------------------------------------------------------------------------------------- | ---------------------------------------------------------- | --------------- |
| `MSR-9051-idle-done` [§6.3.13](https://www.rfc-editor.org/rfc/rfc9051.html#section-6.3.13)  | Complete the IDLE continuation and DONE exchange.          | `tested`        |
| `MSR-9051-idle-renew` [§6.3.13](https://www.rfc-editor.org/rfc/rfc9051.html#section-6.3.13) | Refresh IDLE before server inactivity timeout.             | `tested`        |
| `MSR-9051-move-policy` [§6.4.8](https://www.rfc-editor.org/rfc/rfc9051.html#section-6.4.8)  | Do not silently substitute non-atomic COPY/STORE for MOVE. | `tested`        |
| `MSR-9051-uid-expunge` [§6.4.9](https://www.rfc-editor.org/rfc/rfc9051.html#section-6.4.9)  | Expunge only selected UIDs when UIDPLUS permits it.        | `tested`        |
| `MSR-9051-no-uidplus` [§6.4.9](https://www.rfc-editor.org/rfc/rfc9051.html#section-6.4.9)   | Refuse permanent deletion without safe targeted expunge.   | `tested`        |
| `MSR-9051-rev2-enable` [§6.3.1](https://www.rfc-editor.org/rfc/rfc9051.html#section-6.3.1)  | Full IMAP4rev2 ENABLE and required extension profile.      | `unsupported`   |

## SMTP submission

Primary reference: [RFC 5321](https://www.rfc-editor.org/rfc/rfc5321.html).

| Requirement ID and clause                                                                             | Scoped behavior                                              | Evidence status           |
| ----------------------------------------------------------------------------------------------------- | ------------------------------------------------------------ | ------------------------- |
| `MSR-5321-multiline` [§4.2](https://www.rfc-editor.org/rfc/rfc5321.html#section-4.2)                  | Read complete multiline replies before advancing.            | `tested`                  |
| `MSR-5321-recipients` [§4.1.1.3](https://www.rfc-editor.org/rfc/rfc5321.html#section-4.1.1.3)         | Use RCPT envelope recipients independent of visible headers. | `tested`                  |
| `MSR-5321-data` [§4.1.1.4](https://www.rfc-editor.org/rfc/rfc5321.html#section-4.1.1.4)               | Wait for DATA permission before uploading message bytes.     | `tested`                  |
| `MSR-5321-transparency` [§4.5.2](https://www.rfc-editor.org/rfc/rfc5321.html#section-4.5.2)           | Dot-stuff leading dots and terminate DATA correctly.         | `tested`                  |
| `MSR-5321-uncertain` [§4.1.1.4](https://www.rfc-editor.org/rfc/rfc5321.html#section-4.1.1.4)          | Missing final acceptance cannot prove non-delivery.          | `tested`                  |
| `MSR-5321-path-length` [§4.5.3.1.3](https://www.rfc-editor.org/rfc/rfc5321.html#section-4.5.3.1.3)    | Observe SMTP path length limits.                             | `tested`                  |
| `MSR-5321-command-length` [§4.5.3.1.4](https://www.rfc-editor.org/rfc/rfc5321.html#section-4.5.3.1.4) | Observe SMTP command line octet limits.                      | `tested`                  |
| `MSR-5321-reply-length` [§4.5.3.1.5](https://www.rfc-editor.org/rfc/rfc5321.html#section-4.5.3.1.5)   | Handle maximum SMTP reply lines without unbounded input.     | `intentionally_tolerated` |
| `MSR-5321-text-line` [§4.5.3.1.6](https://www.rfc-editor.org/rfc/rfc5321.html#section-4.5.3.1.6)      | Keep transmitted message text lines within allowed limits.   | `tested`                  |
| `MSR-5321-timeouts` [§4.5.3.2](https://www.rfc-editor.org/rfc/rfc5321.html#section-4.5.3.2)           | Apply bounded SMTP command and DATA timeouts.                | `intentionally_tolerated` |
| `MSR-5321-ehlo-fallback` [§4.1.4](https://www.rfc-editor.org/rfc/rfc5321.html#section-4.1.4)          | Handle non-ESMTP HELO-only servers.                          | `unsupported`             |

## Message format

Primary reference: [RFC 5322](https://www.rfc-editor.org/rfc/rfc5322.html).

| Requirement ID and clause                                                                       | Scoped behavior                                                                              | Evidence status           |
| ----------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------- | ------------------------- |
| `MSR-5322-header-injection` [§2.2](https://www.rfc-editor.org/rfc/rfc5322.html#section-2.2)     | Reject CRLF in tested recipients, subject and reply headers; encode the attachment filename. | `tested`                  |
| `MSR-5322-bcc` [§3.6.3](https://www.rfc-editor.org/rfc/rfc5322.html#section-3.6.3)              | Remove Bcc fields from the transmitted copy.                                                 | `tested`                  |
| `MSR-5322-unfold` [§2.2.3](https://www.rfc-editor.org/rfc/rfc5322.html#section-2.2.3)           | Unfold received recipient headers.                                                           | `tested`                  |
| `MSR-5322-duplicate-singletons` [§3.6](https://www.rfc-editor.org/rfc/rfc5322.html#section-3.6) | Choose the first duplicate singleton value.                                                  | `intentionally_tolerated` |
| `MSR-5322-date` [§3.6.1](https://www.rfc-editor.org/rfc/rfc5322.html#section-3.6.1)             | Use valid message date without inventing a receipt time.                                     | `tested`                  |
| `MSR-5322-reply-headers` [§3.6.4](https://www.rfc-editor.org/rfc/rfc5322.html#section-3.6.4)    | Canonicalize identification and reply threading fields.                                      | `tested`                  |
| `MSR-5322-line-limits` [§2.1.1](https://www.rfc-editor.org/rfc/rfc5322.html#section-2.1.1)      | Enforce 998-character hard line limits and folding recommendations on generated headers.     | `intentionally_tolerated` |
| `MSR-5322-required-fields` [§3.6](https://www.rfc-editor.org/rfc/rfc5322.html#section-3.6)      | Emit exactly one Date and From in newly constructed messages.                                | `tested`                  |
| `MSR-5322-obsolete-input` [§4](https://www.rfc-editor.org/rfc/rfc5322.html#section-4)           | Handle obsolete received syntax without emitting it.                                         | `intentionally_tolerated` |

## SMTPUTF8

Primary reference: [RFC 6531](https://www.rfc-editor.org/rfc/rfc6531.html).

| Requirement ID and clause                                                                       | Scoped behavior                                                           | Evidence status |
| ----------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------- | --------------- |
| `MSR-6531-capability` [§3.2](https://www.rfc-editor.org/rfc/rfc6531.html#section-3.2)           | Do not submit internationalized mail without SMTPUTF8.                    | `tested`        |
| `MSR-6531-parameter` [§3.4](https://www.rfc-editor.org/rfc/rfc6531.html#section-3.4)            | Declare SMTPUTF8 for the tested internationalized envelope.               | `tested`        |
| `MSR-6531-idna` [§3.2](https://www.rfc-editor.org/rfc/rfc6531.html#section-3.2)                 | Use IDNA-compatible processing for internationalized domain lookup.       | `unsupported`   |
| `MSR-6531-utf8-replies` [§3.7.4.1](https://www.rfc-editor.org/rfc/rfc6531.html#section-3.7.4.1) | Handle UTF-8 reply strings and internationalized address rejection codes. | `tested`        |

## SMTP SIZE

Primary reference: [RFC 1870](https://www.rfc-editor.org/rfc/rfc1870.html).

| Requirement ID and clause                                                      | Scoped behavior                                                    | Evidence status |
| ------------------------------------------------------------------------------ | ------------------------------------------------------------------ | --------------- |
| `MSR-1870-octets` [§5](https://www.rfc-editor.org/rfc/rfc1870.html#section-5)  | Declare exact message octets including CRLF for the named fixture. | `tested`        |
| `MSR-1870-maximum` [§6](https://www.rfc-editor.org/rfc/rfc1870.html#section-6) | Refuse a message above the advertised fixed maximum.               | `tested`        |
| `MSR-1870-zero` [§3](https://www.rfc-editor.org/rfc/rfc1870.html#section-3)    | Interpret SIZE zero as no fixed maximum.                           | `tested`        |

## MIME fields and transfer encoding

Primary reference: [RFC 2045](https://www.rfc-editor.org/rfc/rfc2045.html).

| Requirement ID and clause                                                                    | Scoped behavior                                                                | Evidence status |
| -------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------ | --------------- |
| `MSR-2045-quoted-printable` [§6.7](https://www.rfc-editor.org/rfc/rfc2045.html#section-6.7)  | Decode valid quoted-printable text and charset.                                | `tested`        |
| `MSR-2045-base64` [§6.8](https://www.rfc-editor.org/rfc/rfc2045.html#section-6.8)            | Decode valid base64 bodies into original content.                              | `tested`        |
| `MSR-2045-unknown-transfer` [§6.4](https://www.rfc-editor.org/rfc/rfc2045.html#section-6.4)  | Do not display undecodable transfer syntax as message content.                 | `tested`        |
| `MSR-2045-unknown-as-binary` [§6.4](https://www.rfc-editor.org/rfc/rfc2045.html#section-6.4) | Treat unrecognized transfer encodings as opaque application/octet-stream.      | `tested`        |
| `MSR-2045-default-type` [§5.2](https://www.rfc-editor.org/rfc/rfc2045.html#section-5.2)      | Apply default text/plain US-ASCII when Content-Type is absent.                 | `tested`        |
| `MSR-2045-default-transfer` [§6.1](https://www.rfc-editor.org/rfc/rfc2045.html#section-6.1)  | Apply 7bit default when Content-Transfer-Encoding is absent.                   | `tested`        |
| `MSR-2045-parameter-case` [§5.1](https://www.rfc-editor.org/rfc/rfc2045.html#section-5.1)    | Treat media type tokens and parameter names case-insensitively.                | `tested`        |
| `MSR-2045-mime-version` [§4](https://www.rfc-editor.org/rfc/rfc2045.html#section-4)          | Generate MIME-Version 1.0 for MIME messages.                                   | `tested`        |
| `MSR-2045-qp-boundaries` [§6.7](https://www.rfc-editor.org/rfc/rfc2045.html#section-6.7)     | Handle soft breaks, trailing whitespace and malformed QP at every parse path.  | `tested`        |
| `MSR-2045-base64-boundaries` [§6.8](https://www.rfc-editor.org/rfc/rfc2045.html#section-6.8) | Handle padding, whitespace and invalid base64 consistently across parse paths. | `tested`        |

## Multipart and message media

Primary reference: [RFC 2046](https://www.rfc-editor.org/rfc/rfc2046.html).

| Requirement ID and clause                                                                        | Scoped behavior                                                  | Evidence status           |
| ------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------- | ------------------------- |
| `MSR-2046-nested` [§5.1.2](https://www.rfc-editor.org/rfc/rfc2046.html#section-5.1.2)            | Keep nested multipart boundaries out of visible content.         | `tested`                  |
| `MSR-2046-alternative` [§5.1.4](https://www.rfc-editor.org/rfc/rfc2046.html#section-5.1.4)       | Prefer a supported alternative while retaining a valid fallback. | `tested`                  |
| `MSR-2046-mixed` [§5.1.3](https://www.rfc-editor.org/rfc/rfc2046.html#section-5.1.3)             | Retain distinct mixed-branch bodies and attachments.             | `tested`                  |
| `MSR-2046-forwarded` [§5.2.1](https://www.rfc-editor.org/rfc/rfc2046.html#section-5.2.1)         | Keep an attached message separate from its enclosing body.       | `tested`                  |
| `MSR-2046-truncated` [§5.1.1](https://www.rfc-editor.org/rfc/rfc2046.html#section-5.1.1)         | Render a bounded decoded leaf from a truncated multipart.        | `intentionally_tolerated` |
| `MSR-2046-boundary-limit` [§5.1.1](https://www.rfc-editor.org/rfc/rfc2046.html#section-5.1.1)    | Validate multipart boundary syntax, quoting and maximum length.  | `tested`                  |
| `MSR-2046-digest` [§5.1.5](https://www.rfc-editor.org/rfc/rfc2046.html#section-5.1.5)            | Apply message/rfc822 default inside multipart/digest.            | `tested`                  |
| `MSR-2046-unknown-multipart` [§5.1.7](https://www.rfc-editor.org/rfc/rfc2046.html#section-5.1.7) | Treat unrecognized multipart subtypes as mixed.                  | `tested`                  |
| `MSR-2046-default-charset` [§4.1.2](https://www.rfc-editor.org/rfc/rfc2046.html#section-4.1.2)   | Use US-ASCII as the default text charset.                        | `intentionally_tolerated` |

## Encoded header words

Primary reference: [RFC 2047](https://www.rfc-editor.org/rfc/rfc2047.html).

| Requirement ID and clause                                                                  | Scoped behavior                                                         | Evidence status           |
| ------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------- | ------------------------- |
| `MSR-2047-q-header` [§6.2](https://www.rfc-editor.org/rfc/rfc2047.html#section-6.2)        | Decode Q-encoded header display text.                                   | `tested`                  |
| `MSR-2047-b-filename` [§4.1](https://www.rfc-editor.org/rfc/rfc2047.html#section-4.1)      | Decode governed encoded-word filename samples.                          | `intentionally_tolerated` |
| `MSR-2047-placement` [§5](https://www.rfc-editor.org/rfc/rfc2047.html#section-5)           | Restrict encoded-word interpretation to permitted header locations.     | `intentionally_tolerated` |
| `MSR-2047-adjacent-words` [§6.2](https://www.rfc-editor.org/rfc/rfc2047.html#section-6.2)  | Suppress separator whitespace between adjacent encoded words correctly. | `tested`                  |
| `MSR-2047-encoded-word-length` [§2](https://www.rfc-editor.org/rfc/rfc2047.html#section-2) | Generate encoded words no longer than 75 characters.                    | `tested`                  |

## Extended MIME parameters

Primary reference: [RFC 2231](https://www.rfc-editor.org/rfc/rfc2231.html).

| Requirement ID and clause                                                                    | Scoped behavior                                                    | Evidence status           |
| -------------------------------------------------------------------------------------------- | ------------------------------------------------------------------ | ------------------------- |
| `MSR-2231-continuations` [§3](https://www.rfc-editor.org/rfc/rfc2231.html#section-3)         | Decode the governed two-segment filename continuation.             | `tested`                  |
| `MSR-2231-extended` [§4](https://www.rfc-editor.org/rfc/rfc2231.html#section-4)              | Decode extended filename charset and percent octets.               | `tested`                  |
| `MSR-2231-language` [§4](https://www.rfc-editor.org/rfc/rfc2231.html#section-4)              | Accept language metadata without changing decoded filename bytes.  | `tested`                  |
| `MSR-2231-invalid-continuations` [§3](https://www.rfc-editor.org/rfc/rfc2231.html#section-3) | Define missing, duplicate and mixed continuation segment behavior. | `intentionally_tolerated` |

## Flowed text

Primary reference: [RFC 3676](https://www.rfc-editor.org/rfc/rfc3676.html).

| Requirement ID and clause                                                                | Scoped behavior                                          | Evidence status |
| ---------------------------------------------------------------------------------------- | -------------------------------------------------------- | --------------- |
| `MSR-3676-delsp` [§4.2](https://www.rfc-editor.org/rfc/rfc3676.html#section-4.2)         | Join flowed lines with DelSp=yes.                        | `tested`        |
| `MSR-3676-delsp-default` [§4.2](https://www.rfc-editor.org/rfc/rfc3676.html#section-4.2) | Keep the joining space when DelSp is absent or no.       | `tested`        |
| `MSR-3676-signature` [§4.3](https://www.rfc-editor.org/rfc/rfc3676.html#section-4.3)     | Preserve signature separator as a fixed line.            | `tested`        |
| `MSR-3676-unstuff` [§4.4](https://www.rfc-editor.org/rfc/rfc3676.html#section-4.4)       | Remove one stuffing space after quote-depth recognition. | `tested`        |
| `MSR-3676-quotes` [§4.5](https://www.rfc-editor.org/rfc/rfc3676.html#section-4.5)        | Do not join flowed lines across different quote depths.  | `tested`        |

## One-click unsubscribe

Primary reference: [RFC 8058](https://www.rfc-editor.org/rfc/rfc8058.html).

| Requirement ID and clause                                                             | Scoped behavior                                                           | Evidence status |
| ------------------------------------------------------------------------------------- | ------------------------------------------------------------------------- | --------------- |
| `MSR-8058-post` [§3.2](https://www.rfc-editor.org/rfc/rfc8058.html#section-3.2)       | Send only the one-click form key/value by HTTPS POST.                     | `tested`        |
| `MSR-8058-consent` [§3.2](https://www.rfc-editor.org/rfc/rfc8058.html#section-3.2)    | Do not POST before explicit user action.                                  | `tested`        |
| `MSR-8058-dkim` [§4](https://www.rfc-editor.org/rfc/rfc8058.html#section-4)           | Require valid DKIM coverage of both unsubscribe fields.                   | `tested`        |
| `MSR-8058-redirect` [§3.1](https://www.rfc-editor.org/rfc/rfc8058.html#section-3.1)   | Do not follow an unsubscribe redirect.                                    | `tested`        |
| `MSR-8058-no-cookies` [§3.1](https://www.rfc-editor.org/rfc/rfc8058.html#section-3.1) | Do not include cookies or contextual authorization in one-click requests. | `tested`        |

## Mailto

Primary reference: [RFC 6068](https://www.rfc-editor.org/rfc/rfc6068.html).

| Requirement ID and clause                                                                 | Scoped behavior                                                                 | Evidence status |
| ----------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------- | --------------- |
| `MSR-6068-percent` [§2](https://www.rfc-editor.org/rfc/rfc6068.html#section-2)            | Percent-decode mailto fields without treating plus as a space.                  | `tested`        |
| `MSR-6068-fields` [§2](https://www.rfc-editor.org/rfc/rfc6068.html#section-2)             | Parse supported to/cc/subject/body compose fields.                              | `tested`        |
| `MSR-6068-unsafe-headers` [§3](https://www.rfc-editor.org/rfc/rfc6068.html#section-3)     | Reject header-injection attempts in backend mailto actions.                     | `tested`        |
| `MSR-6068-frontend-injection` [§2](https://www.rfc-editor.org/rfc/rfc6068.html#section-2) | Reject CR/LF header injection from every frontend compose mailto path.          | `tested`        |
| `MSR-6068-body-crlf` [§2](https://www.rfc-editor.org/rfc/rfc6068.html#section-2)          | Convert percent-encoded body line endings consistently without double decoding. | `tested`        |
| `MSR-6068-no-send` [§3](https://www.rfc-editor.org/rfc/rfc6068.html#section-3)            | Opening mailto requires review before sending.                                  | `tested`        |

## TLS minimum policy

Primary reference: [RFC 8997](https://www.rfc-editor.org/rfc/rfc8997.html).

| Requirement ID and clause                                                                 | Scoped behavior                                                            | Evidence status |
| ----------------------------------------------------------------------------------------- | -------------------------------------------------------------------------- | --------------- |
| `MSR-8997-min-version` [§3](https://www.rfc-editor.org/rfc/rfc8997.html#section-3)        | Require TLS 1.2 or later for protected email access and submission.        | `tested`        |
| `MSR-8997-confidentiality-ui` [§3](https://www.rfc-editor.org/rfc/rfc8997.html#section-3) | Do not indicate confidentiality without TLS 1.2 and verified certificates. | `unsupported`   |

## Profile decisions and remaining limits

Two conditional roles are explicitly excluded. Dakia submits to a configured SMTP
server and does not resolve recipient domains or perform direct MX delivery, so
the recipient-domain IDNA lookup obligation is outside this submission profile.
This does not certify internationalized configured SMTP hostnames. The UI also
has no live connection confidentiality indicator. Its account TLS configuration
labels are not runtime security evidence. A future direct-delivery feature or
live confidentiality indicator requires its own tests before expanding the profile.

The SMTP reply input test accepts a 1000-octet greeting and rejects 1001 before
EHLO through the production Lettre connection. This is a bounded tolerance for
oversized input. RFC 5321 section 4.5.3.1.5 limits server reply lines to 512 octets;
the test does not replace that limit. The valid 251/551 fixtures use ASCII prose
with a non-ASCII forwarding mailbox: 251 proceeds directly to DATA without a new
recipient command, while 551 stops without submitting. A separate 553 fixture
with UTF-8 prose proves safe failure for malformed server input; arbitrary
non-ASCII reply prose is not permitted by SMTPUTF8.

The current IMAP search slice rejects CR/LF/NUL in query text and configured or
requested mailbox names before opening a connection, preserves scoped cached
messages, and reports SELECT refusal instead of presenting a successful empty
search. The exact public-boundary assertions are
`remote_search_rejects_query_command_controls_without_writing_or_losing_state`,
`remote_search_rejects_configured_mailbox_controls_without_writing_or_losing_state`,
`remote_search_rejects_requested_mailbox_controls_without_connecting_or_losing_state`,
and `remote_search_reports_scoped_and_unified_select_refusal_without_searching_or_losing_state`
in the [remote search tests](../crates/dakia-core/src/mail/tests/remote_search_safety_tests.rs).
Unicode SEARCH now uses an exact UTF-8 octet-counted synchronizing literal and
waits for server continuation before sending its bytes. The Unicode, quote and
backslash test checks account/mailbox/limit isolation. BADCHARSET before or after
continuation yields a bounded error without followup fetching and preserves cache
and catalogue fields after SQLite reopen. The production connector refuses
`UTF8=ONLY` before LOGIN; it retains compatibility with `UTF8=ACCEPT` through its
IMAP4rev1 path. This does not claim full RFC 6855 or IMAP4rev2 support.

Additional [production command tests](../crates/dakia-core/src/mail/tests/imap_command_construction_tests.rs)
exercise password and raw OAuth credential control rejection, tagged command and
APPEND mailbox framing rejection, and exact literal-byte preservation.
`authentication_rejection_does_not_retain_an_echoed_secret_in_the_error_chain`
and `authentication_bye_does_not_retain_echoed_credentials_in_retryable_errors`
assert safe password/token errors through displayed text, debug formatting and
error chains. `generic_command_rejections_do_not_retain_provider_echoes` and
`append_rejections_do_not_retain_echoed_mailbox_or_message_content` cover bounded
errors without server-echoed private content. These tests use the shared production
command methods over a controlled duplex transport. They support Dakia privacy
policy; they do not claim an RFC mandates a particular error message or prove that
every logging or telemetry path has been checked.

Modified UTF-7 now has canonical codec vectors and production TLS sync, MOVE and
Sent APPEND evidence preserving exact wire identity through reopen. Noncanonical
LIST names fail before later commands and preserve the prior cache. Canonical-looking
ASCII account configuration is deliberately interpreted as previous-stable wire
format, so that dual-format ambiguity remains explicit until a representation
marker exists. The finite LIST profile now covers quoted and atom names, bounded
mailbox literals, NIL/dot/slash and escaped delimiters, literal-to-response binding,
unknown attributes and authoritative NoSelect safety. Production TLS sync retains
literal wire identity through Store reopen; known NoSelect targets and malformed
LIST responses cannot reach select or mutation commands. Mailbox literals above
8 KiB are rejected before their payload is read. Dakia does not derive hierarchical
paths from delimiters in this profile; NAMESPACE, full IMAP4rev2, UTF8 ENABLE,
arbitrary response literals, independent-server, provider and native acceptance
are not claimed by these source tests.

Password IMAP accounts use LOGIN only when it is permitted by post-TLS
capabilities. Existing OAuth accounts use XOAUTH2 with SASL-IR. General SASL
mechanism selection is not implemented. Dakia deliberately refuses a non-atomic
COPY/STORE substitute for unavailable MOVE, and permanent deletion requires a
safe targeted expunge path. These are product safety decisions, not claims that
base IMAP requires those policies.

SMTP submission now has separate connect, greeting, TLS, EHLO, STARTTLS, AUTH,
MAIL, each RCPT, DATA initiation, DATA progress, terminator and final-response
budgets, with no aggregate transaction ceiling. Production defaults follow the
RFC delivery recommendations: greeting/MAIL/RCPT 5 minutes, DATA initiation 2
minutes, successful DATA write progress 3 minutes and final acknowledgement 10
minutes. The DATA codec consumes 16 KiB input chunks. The terminator has a separate 3-minute budget. Core
TLS tests prove the production path uses that policy and each recipient gets a
fresh command budget; the patched transport tests prove progress, stalled
terminator and final-response behavior. Missing final acknowledgement or uncertain
terminator completion remains delivery-uncertain. Runtime configurability is an
explicit tolerated SHOULD deviation because the intervals are internal constants.
The separate authentication-only probe retains a 20-second deadline and never
reaches MAIL, RCPT or DATA. This source profile does not prove real external DNS
stalls, provider behavior or native UI.

The nested production-send fixture also proves CRLF-only wire data and the
mandatory 998-octet generated-header limit. Preferred 78-character folding remains
a narrow tested compatibility tolerance: exactly the encoded To/Cc lines exceed
78 (observed 91-101), and neither may exceed 110. This does not claim all generated
headers follow the preferred width. Exact decoded From/To/Cc address lists and
Subject survive the SMTP capture; Bcc remains envelope-only. RFC 2231 filename
parameters preserve the exact long Unicode filename and attachment bytes after
production reparsing. Existing direct compose tests separately count exactly one
Date and From field.

A maximum-size SMTP test verifies the named raw message's octets and boundary
acceptance. A dot-leading message tests transparency separately. The matrix does
not treat these as proof of every combined SIZE/transparency vector. Internationalized
submission assertions cover their named envelopes, bodies and forwarding replies;
they do not certify general IDNA conversion or every SMTPUTF8 reply form.

MIME assertions range from direct production decoder checks to complete parse and
publication paths. The flowed decoder tests prove exact text transformations, not
native Reader layout. The nine-fixture artifact lane supplies additional
core-to-SQLite-to-command-to-Reader evidence for its named fixture shapes. Neither
boundary establishes every MIME clause. The new governed MIME vectors cover absent header defaults, mixed-case tokens,
adjacent encoded words, digest children, ordered unknown multipart children and
RFC 2231 filename recovery. Unknown transfer encodings now preserve named and
unnamed parts as opaque binary attachments, including targeted real-TLS downloads,
instead of losing bytes or rendering their apparent HTML. Valid quoted-printable
soft breaks and trailing whitespace now preserve exact text through complete and
selective publication and SQLite reopen. Standalone illegal `=QZ` is rejected
through complete parsing and real-TLS hydration while preserving neighboring cached
mail after reopen. That rejection is Dakia policy, not an RFC-mandated response to
illegal input. Canonical base64 final quanta and transport whitespace pass the
complete/selective/reopen paths. Derived malformed base64 vectors now reject
invalid characters, truncated quanta, missing padding and misplaced padding through
complete parsing and production TLS hydration, without publishing partial cache or
changing recoverable catalogue fields. All fields of valid neighboring summaries
and cached content survive reopen. Rejection is Dakia policy, not an RFC-mandated
response to every malformed transfer. Excess terminal padding (`QQ===`, `QUI==`)
is an intentionally tested receive tolerance that must preserve exact `A`/`AB`
bytes across complete parsing, TLS-selective publication and reopen. It does not
make those inputs canonical RFC 2045 output. Received multipart boundaries cover
quoted punctuation and lengths 69 and 70, with length 71 explicitly tolerated.
Actual MailService SMTP DATA capture now proves three unique, quoted generated
mixed/alternative/related boundaries use the allowed ASCII grammar, remain at most
70 characters and have matching delimiters. Reparsed part cardinalities, plain
fallback, HTML-to-inline-image Content-ID linkage, PDF filename and exact bytes
remain intact. A missing closing delimiter fails the dedicated sensitivity check.
Absent-charset ASCII remains exact; valid unlabeled UTF-8 is deliberately tolerated
with exact complete/selective decoding parity after 8bit, quoted-printable and
base64 transport decoding. Invalid bytes reject without replacement. This is a
complete-parse/selective-decoder assertion, not a TLS hydration or reopen claim. Raw encoded-looking mailbox and threading identities now remain literal across
header, catalogue, complete and threading paths, including first-mailbox and
quoted-display bracket regressions. Allowed display and Subject words still decode.
Strict RFC 2047 placement is not claimed: encoded-word filenames remain a tested
interoperability tolerance and vendor display parsing accepts quoted display syntax.
The finite obsolete-receive corpus now covers 19 families in 48 governed fictional
fixtures, with 51 passing source tests for complete/header/catalogue parsing,
Store/cache reopen, exact raw TLS export, modern generated reply headers and a
CTL-bearing target refused before SMTP. Literal control text is deliberately
preserved as source data without prescribing visible glyphs. Military zones use
-0000-equivalent UTC instants; unknown-offset provenance is not represented.
Ambiguous quoted-local routes remain literal rather than risking identity changes;
that preservation does not prove those routes are themselves send-rejected. The
exact send-refusal assertion applies to the CTL-bearing domain target. These are
reviewed finite-profile tolerances, not exhaustive grammar or all-combination
conformance. Frontend Reader, Tauri, WebKit, native, provider, clipboard and search
acceptance remain separate evidence. The source result is recorded in
[the bounded receive report](mail-safety-results/obsolete-receive-source-2026-10-21.json).

The opaque-attachment over-limit helper test proves only its local
guard; it does not prove that an oversized real provider fetch reaches that guard.

One-click tests verify DKIM validity and required header coverage, explicit-action
request ordering, the exact form payload, absent context headers, pinned destinations,
and redirect refusal at their named source boundaries. The fictional TXT-cache path
is not live DNS/provider acceptance. Native Reader consent and network observations
remain separate evidence.

The shipped frontend mailto entrypoint is a Reader email-body link, with no
OS-wide URI registration. The App-to-detached-Reader authenticated event and opaque
native compose-seed path now covers independent CR/LF injection in To/Cc/Subject,
disabled Send, click/shortcut refusal and no external URL opening. Double-encoded
fields decode once. Actual body CRLF survives in the seed and normalizes to LF in
the rich-text send payload while literal `%0D%0A` text remains unchanged. An
explicit pre-interaction helper assertion proves ComposeApp does not send merely
because the link opened. These source assertions do not replace physical WebKit
activation acceptance.

TLS trust and hostname rejection are tested through production constructors.
Constrained real servers now prove exact IMAP TLS 1.2/1.3 acceptance and SMTP
implicit/STARTTLS TLS 1.2/1.3 acceptance using their shared production profile
factories. The SMTP paths authenticate and quit over the observed version.
Separate manually invoked IMAP and SMTP tests first negotiate an actual OpenSSL
TLS 1.1 control, then prove Dakia refuses an identical legacy-only implicit-TLS
server before credentials or SMTP envelope commands. The default source run has
six passes and two ignored tests; the two explicit legacy passes are separate
manual evidence, never default passes. Direct legacy-version refusal is implicit
TLS only. STARTTLS has positive 1.2/1.3 negotiation through the same production
parameters and separate certificate-refusal evidence, not an explicit TLS 1.1
downgrade transcript. The reviewed results are recorded in
[TLS version evidence](mail-safety-results/tls-version-profile-2026-10-21.json).
No general cipher-suite, certificate revocation, or client-certificate conformance
is claimed.

SMTP relay routing, MX selection, store-and-forward queues, server-only IMAP
obligations, and operation of a mailing-list sender's unsubscribe endpoint are outside
this mail user agent profile. New shipped capabilities or newly encountered inputs
must extend the matrix before making a broader support claim. The profile's current
gaps prevent a claim that every applicable standards requirement has passed.

Provider, native, performance, fuzzing, updater, and release results are recorded
separately in [mail safety evidence](mail-safety-evidence.md). The
[remaining scenario gaps](mail-safety-remaining-gaps.md) retain the programme's
aggregate requirements and acceptance lanes.
