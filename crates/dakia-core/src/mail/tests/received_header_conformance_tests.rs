//! Focused RFC 5322, RFC 2047, and RFC 2046 receive conformance boundaries.
//!
//! These inputs are redacted derivatives of the RFC examples and grammars:
//! - RFC 5322 section 3.6.7 and section 4.4 / `obs-route`
//! - RFC 2047 section 5 and section 8
//! - RFC 2046 section 4.1.2
//!
//! The tests deliberately keep obsolete syntax at the receive boundary. The
//! composer assertion separately proves that Dakia emits an ordinary current
//! mailbox and never manufactures trace fields.

use super::*;

const RFC5322_OBS_ROUTE_RECEIVED: &[u8] = concat!(
    "Received: from legacy.example (legacy.example [192.0.2.10])\r\n",
    "\tby relay.example with ESMTP id fictional-42\r\n",
    "\tfor <@route-a.example,@route-b.example:reader@example.test>;\r\n",
    "\tTue, 28 Jul 2026 12:00:00 +0000\r\n",
    "Date: Tue, 28 Jul 2026 12:01:00 +0000\r\n",
    "From: Sender <sender@example.test>\r\n",
    "To: Reader <reader@example.test>\r\n",
    "Subject: folded received boundary\r\n",
    "Message-ID: <received-boundary@example.test>\r\n",
    "Content-Type: text/plain; charset=us-ascii\r\n",
    "\r\n",
    "A plain body.\r\n",
)
.as_bytes();

const RFC2047_ALLOWED_DISPLAY_WORDS: &[u8] = concat!(
    "Date: Tue, 28 Jul 2026 12:01:00 +0000\r\n",
    // RFC 2047 section 8's display-name form, with a current RFC 5322
    // envelope and a UTF-8 subject for the Reader-visible assertion.
    "From: =?US-ASCII?Q?Keith_Moore?= <moore@example.test>\r\n",
    "To: =?UTF-8?Q?Reader_Display?= <reader@example.test>\r\n",
    "Cc: Team: Alice <alice@example.test>, Bob <bob@example.test>;\r\n",
    "Bcc: Good <good@example.test>, malformed text\r\n",
    "Subject: =?UTF-8?Q?caf=C3=A9?=\r\n",
    "Message-ID: <allowed-encoded-word@example.test>\r\n",
    "Content-Type: text/plain; charset=utf-8\r\n",
    "\r\n",
    "display-safe body\r\n",
)
.as_bytes();

const RFC2047_FORBIDDEN_STRUCTURED_WORDS: &[u8] = concat!(
    "Date: Tue, 28 Jul 2026 12:01:00 +0000\r\n",
    "From: Sender <=?UTF-8?Q?attacker?=@example.test\r\n",
    "To: =?UTF-8?Q?To_Display?= <to=?UTF-8?Q?shadow?=@example.test>\r\n",
    "Cc: =?UTF-8?Q?Cc_Display?= <cc=?UTF-8?Q?shadow?=@example.test>\r\n",
    "Bcc: =?UTF-8?Q?Bcc_Display?= <bcc=?UTF-8?Q?shadow?=@example.test>\r\n",
    "Reply-To: =?UTF-8?Q?Reply_Display?= <reply=?UTF-8?Q?shadow?=@example.test>\r\n",
    "Subject: structured identity boundary\r\n",
    "Message-ID: <=?UTF-8?Q?message?=@example.test>\r\n",
    "In-Reply-To: <=?UTF-8?Q?reply?=@example.test>\r\n",
    "References: <=?UTF-8?Q?reference?=@example.test>\r\n",
    "Content-Type: text/plain; charset=us-ascii\r\n",
    "\r\n",
    "identity body\r\n",
)
.as_bytes();

fn conformance_account() -> Account {
    let mut account = test_account();
    account.id = uuid::Uuid::parse_str("73737373-7373-4737-8737-737373737373")
        .expect("fixed conformance account UUID");
    account.email = "modern@example.test".into();
    account.display_name = "Modern Sender".into();
    account
}

#[tokio::test]
async fn rfc5322_obs_route_and_folded_received_are_receive_only() {
    let account = conformance_account();
    let parsed = parse_header_block(RFC5322_OBS_ROUTE_RECEIVED)
        .expect("folded trace header must not prevent header parsing");
    assert!(
        parsed.header(HeaderName::Received).is_some(),
        "the folded trace field must remain present even when an obsolete route prevents detailed trace parsing"
    );

    let headers = parse_header_message(&account, "INBOX", 701, &[], RFC5322_OBS_ROUTE_RECEIVED)
        .expect("header-only path accepts obsolete route syntax in Received");
    let complete = parse_message(
        &account,
        "INBOX",
        701,
        &[],
        RFC5322_OBS_ROUTE_RECEIVED,
        None,
    )
    .await
    .expect("complete path accepts folded Received syntax");

    assert_eq!(headers.from_address, "sender@example.test");
    assert_eq!(complete.from_address, "sender@example.test");
    assert_eq!(complete.to_addresses, "Reader <reader@example.test>");
    assert_eq!(complete.subject, "folded received boundary");
    assert_eq!(
        complete.received_at,
        chrono::DateTime::parse_from_rfc2822("Tue, 28 Jul 2026 12:01:00 +0000")
            .expect("fixture date")
            .with_timezone(&Utc),
        "the user-visible time comes from Date, not an obsolete Received route"
    );
    assert_eq!(headers.received_at, complete.received_at);

    let draft = ComposeMessage {
        account_id: account.id,
        to: vec!["reader@example.test".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "Modern outbound syntax".into(),
        body_text: "A plain body.".into(),
        body_html: None,
        in_reply_to: None,
        references: None,
        attachments: Vec::new(),
    };
    let source = String::from_utf8(build_compose_message(&account, &draft).unwrap().formatted())
        .expect("composer emits UTF-8 transport bytes");
    let outbound = parse_header_block(source.as_bytes()).expect("composer output is a message");
    let (outbound_name, outbound_address) = parse_first_address(&decoded_header(&outbound, "From"));
    assert_eq!(outbound_name.as_deref(), Some("Modern Sender"));
    assert_eq!(outbound_address, "modern@example.test");
    assert!(!source.contains("Received:"));
    assert!(!source.contains("@route-a.example,"));
}

#[tokio::test]
async fn rfc2047_decodes_only_allowed_display_and_unstructured_words() {
    let account = conformance_account();
    let headers = parse_header_message(&account, "INBOX", 702, &[], RFC2047_ALLOWED_DISPLAY_WORDS)
        .expect("header-only selective metadata path");
    let complete = parse_message(
        &account,
        "INBOX",
        702,
        &[],
        RFC2047_ALLOWED_DISPLAY_WORDS,
        None,
    )
    .await
    .expect("complete Reader path");

    assert_eq!(headers.subject, "café");
    assert_eq!(headers.from_name.as_deref(), Some("Keith Moore"));
    assert_eq!(headers.from_address, "moore@example.test");
    assert_eq!(headers.to_addresses, "Reader Display <reader@example.test>");
    assert_eq!(
        headers.cc_addresses,
        "Team: Alice <alice@example.test>, Bob <bob@example.test>;"
    );
    assert_eq!(
        headers.bcc_addresses,
        "Good <good@example.test>, malformed text"
    );
    assert_eq!(complete.subject, headers.subject);
    assert_eq!(complete.from_name, headers.from_name);
    assert_eq!(complete.from_address, headers.from_address);
    assert_eq!(complete.to_addresses, headers.to_addresses);
    assert_eq!(complete.cc_addresses, headers.cc_addresses);
    assert_eq!(complete.bcc_addresses, headers.bcc_addresses);
    assert_eq!(complete.body_text.trim(), "display-safe body");
}

#[tokio::test]
async fn rfc2047_forbidden_structured_words_cannot_rewrite_mailbox_or_thread_identity() {
    let account = conformance_account();
    let headers = parse_header_message(
        &account,
        "INBOX",
        703,
        &[],
        RFC2047_FORBIDDEN_STRUCTURED_WORDS,
    )
    .expect("invalid encoded words must not block header metadata");
    let complete = parse_message(
        &account,
        "INBOX",
        703,
        &[],
        RFC2047_FORBIDDEN_STRUCTURED_WORDS,
        None,
    )
    .await
    .expect("invalid encoded words must not block body display");

    let expected_from = "=?UTF-8?Q?attacker?=@example.test";
    let expected_to = "=?UTF-8?Q?To_Display?= <to=?UTF-8?Q?shadow?=@example.test>";
    let expected_cc = "=?UTF-8?Q?Cc_Display?= <cc=?UTF-8?Q?shadow?=@example.test>";
    let expected_bcc = "=?UTF-8?Q?Bcc_Display?= <bcc=?UTF-8?Q?shadow?=@example.test>";
    let expected_reply_to = "=?UTF-8?Q?Reply_Display?= <reply=?UTF-8?Q?shadow?=@example.test>";
    let expected_message_id = Some("<=?UTF-8?Q?message?=@example.test>");
    let expected_in_reply_to = Some("<=?UTF-8?Q?reply?=@example.test>");
    let expected_references = Some("<=?UTF-8?Q?reference?=@example.test>");
    let catalogue = parse_catalog_message(
        &account,
        "INBOX",
        703,
        &[],
        RFC2047_FORBIDDEN_STRUCTURED_WORDS,
        "catalogue preview".into(),
    )
    .expect("catalogue path keeps structured identities literal");
    let threading = parse_threading_headers(RFC2047_FORBIDDEN_STRUCTURED_WORDS)
        .expect("threading path keeps structured identities literal");
    let mut rewritten = Vec::new();
    for (path, summary) in [
        ("header-only", &headers),
        ("catalogue", &catalogue),
        ("complete", &complete),
    ] {
        if summary.from_address != expected_from
            || summary.to_addresses != expected_to
            || summary.cc_addresses != expected_cc
            || summary.bcc_addresses != expected_bcc
            || summary.reply_to_addresses != expected_reply_to
            || summary.message_id.as_deref() != expected_message_id
            || summary.in_reply_to.as_deref() != expected_in_reply_to
            || summary.reference_ids.as_deref() != expected_references
        {
            rewritten.push(format!(
                "{path}: from={:?}, to={:?}, cc={:?}, bcc={:?}, reply_to={:?}, message_id={:?}, in_reply_to={:?}, references={:?}",
                summary.from_address,
                summary.to_addresses,
                summary.cc_addresses,
                summary.bcc_addresses,
                summary.reply_to_addresses,
                summary.message_id,
                summary.in_reply_to,
                summary.reference_ids,
            ));
        }
    }
    assert!(
        rewritten.is_empty(),
        "RFC 2047 forbids encoded-word interpretation inside addr-spec and message identifier syntax. A syntactically valid raw token must remain exact rather than become a different identity: {rewritten:?}"
    );
    assert_eq!(threading.message_id.as_deref(), expected_message_id);
    assert_eq!(threading.in_reply_to.as_deref(), expected_in_reply_to);
    assert_eq!(threading.reference_ids.as_deref(), expected_references);
}

#[tokio::test]
async fn rfc2046_absent_charset_is_consistent_and_bounded_across_complete_and_selective_decoding() {
    let account = conformance_account();
    let headers = concat!(
        "Date: Tue, 28 Jul 2026 12:01:00 +0000\r\n",
        "From: Sender <sender@example.test>\r\n",
        "To: Reader <reader@example.test>\r\n",
        "Subject: undeclared charset\r\n",
        "Message-ID: <undeclared-charset@example.test>\r\n",
        "Content-Type: text/plain\r\n",
        "Content-Transfer-Encoding: 8bit\r\n",
        "\r\n",
    );
    let ascii_raw = format!("{headers}plain ASCII\r\n");
    let utf8_raw = format!("{headers}caf\u{00e9}\r\n");
    let mut invalid_raw = headers.as_bytes().to_vec();
    invalid_raw.extend_from_slice(b"broken:\xff\r\n");

    let header_only = parse_header_message(&account, "INBOX", 704, &[], utf8_raw.as_bytes())
        .expect("bad body bytes must not hide header metadata");
    assert_eq!(header_only.subject, "undeclared charset");

    let ascii_complete = parse_message(&account, "INBOX", 704, &[], ascii_raw.as_bytes(), None)
        .await
        .expect("RFC 2046 default US-ASCII remains readable");
    assert_eq!(ascii_complete.body_text, "plain ASCII\r\n");
    assert_eq!(
        decode_mime_part_body(
            b"Content-Type: text/plain\r\nContent-Transfer-Encoding: 8bit\r\n",
            b"plain ASCII\r\n",
        )
        .expect("selective ASCII decoding"),
        ascii_complete.body_text
    );

    // Dakia accepts a valid UTF-8 body when a nonconforming sender omitted its
    // charset. This compatibility tolerance must be exact and identical on
    // complete and selective Reader paths, never a replacement decode.
    let utf8_complete = parse_message(&account, "INBOX", 704, &[], utf8_raw.as_bytes(), None)
        .await
        .expect("bounded valid UTF-8 receive tolerance");
    let utf8_selective = decode_mime_part_body(
        b"Content-Type: text/plain\r\nContent-Transfer-Encoding: 8bit\r\n",
        "caf\u{00e9}\r\n".as_bytes(),
    );
    assert!(
        utf8_selective.is_ok(),
        "valid UTF-8 tolerance must remain available through selective fetch: {utf8_selective:?}"
    );
    assert_eq!(
        utf8_complete.body_text,
        utf8_selective.expect("checked above"),
        "complete and selective paths must publish the same user-visible text"
    );
    assert_eq!(utf8_complete.body_text, "caf\u{00e9}\r\n");

    for (encoding, transport) in [
        ("quoted-printable", b"caf=C3=A9\r\n".as_slice()),
        ("base64", b"Y2Fmw6kNCg==\r\n".as_slice()),
    ] {
        let encoded_headers = headers.replacen("8bit", encoding, 1);
        let mut encoded_raw = encoded_headers.as_bytes().to_vec();
        encoded_raw.extend_from_slice(transport);
        let complete = parse_message(&account, "INBOX", 704, &[], &encoded_raw, None)
            .await
            .expect("valid UTF-8 compatibility after transport decoding");
        let selective = decode_mime_part_body(encoded_headers.as_bytes(), transport)
            .expect("selective transport decoding");
        assert_eq!(complete.body_text, "caf\u{00e9}\r\n", "{encoding} complete");
        assert_eq!(selective, complete.body_text, "{encoding} selective parity");
    }

    let invalid_complete = parse_message(&account, "INBOX", 704, &[], &invalid_raw, None).await;
    let invalid_selective = decode_mime_part_body(
        b"Content-Type: text/plain\r\nContent-Transfer-Encoding: 8bit\r\n",
        b"broken:\xff\r\n",
    );
    assert!(
        invalid_complete.is_err() && invalid_selective.is_err(),
        "invalid undeclared text bytes must fail rather than be replaced or diverge; complete={invalid_complete:?}, selective={invalid_selective:?}"
    );
    for (encoding, transport) in [
        ("quoted-printable", b"broken:=FF\r\n".as_slice()),
        ("base64", b"YnJva2VuOv8NCg==\r\n".as_slice()),
    ] {
        let encoded_headers = headers.replacen("8bit", encoding, 1);
        let mut encoded_raw = encoded_headers.as_bytes().to_vec();
        encoded_raw.extend_from_slice(transport);
        let complete = parse_message(&account, "INBOX", 704, &[], &encoded_raw, None).await;
        let selective = decode_mime_part_body(encoded_headers.as_bytes(), transport);
        assert!(
            complete.is_err() && selective.is_err(),
            "invalid {encoding} bytes must fail without replacement; complete={complete:?}, selective={selective:?}"
        );
    }
}

#[test]
fn rfc5322_duplicate_message_id_uses_only_the_first_structured_header() {
    let raw = concat!(
        "From: Sender <sender@example.test>\r\n",
        "Message-ID: <first@example.test>\r\n",
        "Message-ID: <second@example.test>\r\n",
        "In-Reply-To: <reply@example.test>\r\n",
        "References: <first-reference@example.test>\r\n",
        "References: <second-reference@example.test>\r\n",
        "\r\n",
    );
    let threading = parse_threading_headers(raw.as_bytes()).expect("header parse");
    assert_eq!(
        threading.message_id.as_deref(),
        Some("<first@example.test>")
    );
    assert_eq!(
        threading.in_reply_to.as_deref(),
        Some("<reply@example.test>")
    );
    assert_eq!(
        threading.reference_ids.as_deref(),
        Some("<first-reference@example.test>")
    );
}

#[tokio::test]
async fn rfc5322_first_mailbox_cannot_be_replaced_by_a_later_malformed_sender() {
    let account = conformance_account();
    let raw = concat!(
        "Date: Tue, 28 Jul 2026 12:01:00 +0000\r\n",
        "From: Legit <legit@example.test>, Sender <=?UTF-8?Q?attacker?=@example.test>\r\n",
        "To: Reader <reader@example.test>\r\n",
        "Subject: first sender wins\r\n",
        "Message-ID: <first-from@example.test>\r\n",
        "Content-Type: text/plain; charset=us-ascii\r\n",
        "\r\n",
        "body\r\n",
    );
    let header_only = parse_header_message(&account, "INBOX", 705, &[], raw.as_bytes())
        .expect("header-only parse");
    let catalogue = parse_catalog_message(
        &account,
        "INBOX",
        705,
        &[],
        raw.as_bytes(),
        "preview".into(),
    )
    .expect("catalogue parse");
    let complete = parse_message(&account, "INBOX", 705, &[], raw.as_bytes(), None)
        .await
        .expect("complete parse");
    for (path, summary) in [
        ("header-only", header_only),
        ("catalogue", catalogue),
        ("complete", complete),
    ] {
        assert_eq!(summary.from_name.as_deref(), Some("Legit"), "{path}");
        assert_eq!(summary.from_address, "legit@example.test", "{path}");
    }
}

#[tokio::test]
async fn rfc5322_first_bare_mailbox_cannot_be_replaced_by_a_later_malformed_sender() {
    let account = conformance_account();
    let raw = concat!(
        "Date: Tue, 28 Jul 2026 12:01:00 +0000\r\n",
        "From: first@example.test, =?UTF-8?Q?second?=@second.test\r\n",
        "To: Reader <reader@example.test>\r\n",
        "Subject: first bare sender wins\r\n",
        "Message-ID: <first-bare-from@example.test>\r\n",
        "Content-Type: text/plain; charset=us-ascii\r\n",
        "\r\n",
        "body\r\n",
    );
    let header_only = parse_header_message(&account, "INBOX", 709, &[], raw.as_bytes())
        .expect("header-only parse");
    let catalogue = parse_catalog_message(
        &account,
        "INBOX",
        709,
        &[],
        raw.as_bytes(),
        "preview".into(),
    )
    .expect("catalogue parse");
    let complete = parse_message(&account, "INBOX", 709, &[], raw.as_bytes(), None)
        .await
        .expect("complete parse");
    for (path, summary) in [
        ("header-only", header_only),
        ("catalogue", catalogue),
        ("complete", complete),
    ] {
        assert_eq!(summary.from_name, None, "{path}");
        assert_eq!(summary.from_address, "first@example.test", "{path}");
    }
}

#[tokio::test]
async fn rfc5322_quoted_display_brackets_never_override_the_actual_from_addr_spec() {
    let account = conformance_account();
    let raw = concat!(
        "Date: Tue, 28 Jul 2026 12:01:00 +0000\r\n",
        "From: \"display <victim=?UTF-8?Q?attacker?=@evil.test>\" <real@example.test>\r\n",
        "To: Reader <reader@example.test>\r\n",
        "Subject: quoted display\r\n",
        "Message-ID: <quoted-display@example.test>\r\n",
        "Content-Type: text/plain; charset=us-ascii\r\n",
        "\r\n",
        "body\r\n",
    );
    let header_only = parse_header_message(&account, "INBOX", 706, &[], raw.as_bytes())
        .expect("header-only parse");
    let catalogue = parse_catalog_message(
        &account,
        "INBOX",
        706,
        &[],
        raw.as_bytes(),
        "preview".into(),
    )
    .expect("catalogue parse");
    let complete = parse_message(&account, "INBOX", 706, &[], raw.as_bytes(), None)
        .await
        .expect("complete parse");
    for (path, summary) in [
        ("header-only", header_only),
        ("catalogue", catalogue),
        ("complete", complete),
    ] {
        assert_eq!(summary.from_address, "real@example.test", "{path}");
    }
}

#[tokio::test]
async fn rfc2047_mixed_angle_and_bare_recipients_remain_literal() {
    let account = conformance_account();
    let raw = concat!(
        "Date: Tue, 28 Jul 2026 12:01:00 +0000\r\n",
        "From: Sender <sender@example.test>\r\n",
        "To: Safe <safe@example.test>, =?UTF-8?Q?attacker?=@evil.test\r\n",
        "Subject: mixed recipients\r\n",
        "Message-ID: <mixed-recipients@example.test>\r\n",
        "Content-Type: text/plain; charset=us-ascii\r\n",
        "\r\n",
        "body\r\n",
    );
    let header_only = parse_header_message(&account, "INBOX", 707, &[], raw.as_bytes())
        .expect("header-only parse");
    let catalogue = parse_catalog_message(
        &account,
        "INBOX",
        707,
        &[],
        raw.as_bytes(),
        "preview".into(),
    )
    .expect("catalogue parse");
    let complete = parse_message(&account, "INBOX", 707, &[], raw.as_bytes(), None)
        .await
        .expect("complete parse");
    let expected = "Safe <safe@example.test>, =?UTF-8?Q?attacker?=@evil.test";
    for (path, summary) in [
        ("header-only", header_only),
        ("catalogue", catalogue),
        ("complete", complete),
    ] {
        assert_eq!(summary.to_addresses, expected, "{path}");
    }
}

#[tokio::test]
async fn rfc2047_obsolete_cfws_before_at_remains_a_literal_sender_identity() {
    let account = conformance_account();
    let raw_from = "=?UTF-8?Q?victim?= (note) @example.test";
    let encoded_end = raw_from.find("?=").expect("encoded-word end") + 2;
    assert!(
        cfws_followed_by_at(raw_from.as_bytes(), encoded_end).0,
        "comments and whitespace directly before @ are CFWS"
    );
    assert!(
        forbidden_bare_encoded_word_addr_spec(raw_from),
        "the receive-only obsolete local part must be detected before tolerant parsing"
    );
    let raw = concat!(
        "Date: Tue, 28 Jul 2026 12:01:00 +0000\r\n",
        "From: =?UTF-8?Q?victim?= (note) @example.test\r\n",
        "To: Reader <reader@example.test>\r\n",
        "Subject: obsolete CFWS local part\r\n",
        "Message-ID: <obsolete-cfws@example.test>\r\n",
        "Content-Type: text/plain; charset=us-ascii\r\n",
        "\r\n",
        "body\r\n",
    );

    let header_only = parse_header_message(&account, "INBOX", 708, &[], raw.as_bytes())
        .expect("header-only parse");
    let catalogue = parse_catalog_message(
        &account,
        "INBOX",
        708,
        &[],
        raw.as_bytes(),
        "preview".into(),
    )
    .expect("catalogue parse");
    let complete = parse_message(&account, "INBOX", 708, &[], raw.as_bytes(), None)
        .await
        .expect("complete parse");

    for (path, summary) in [
        ("header-only", header_only),
        ("catalogue", catalogue),
        ("complete", complete),
    ] {
        assert_eq!(
            summary.from_address, "=?UTF-8?Q?victim?= (note) @example.test",
            "{path}"
        );
    }
}

#[test]
fn rfc2047_bare_addr_spec_scanner_stays_bounded_at_header_limit() {
    let mut header = String::from("Safe <safe@example.test>, ");
    while header.len() + b"=?A".len() + b"?=".len() <= MAX_MIME_HEADER_BYTES {
        header.push_str("=?A");
    }
    header.push_str("?=");

    assert!(header.len() <= MAX_MIME_HEADER_BYTES);
    assert!(
        !forbidden_bare_encoded_word_addr_spec(&header),
        "encoded-looking display fragments without an addr-spec continuation are not identities"
    );
}
