//! Fictional RFC 5322 section 4 receive corpus. The checked-in cases.json pins
//! independently chosen oracles, including modern structured identity controls.
//! No test sends mail. Store projections are core Reader payload fields, not a
//! claim that this module renders a WebKit view or executes a frontend reply.
use super::*;
use crate::CachedMessageContent;
use serde_json::{json, Value};
use tokio::{
    io::{split, AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};

const OBSOLETE_EPOCH: i64 = 7_5322;

fn obsolete_spec(name: &str) -> Value {
    let corpus: Value = serde_json::from_str(include_str!(
        "../../../testdata/mime/obsolete-receive/cases.json"
    ))
    .expect("checked-in fictional receive contract");
    corpus["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["id"] == name)
        .unwrap_or_else(|| panic!("unregistered obsolete fixture {name}"))
        .clone()
}

fn metadata_projection(message: &MailSummary) -> Value {
    json!({"account_id":message.account_id,"mailbox":message.mailbox,"uid":message.uid,
        "message_id":message.message_id,"in_reply_to":message.in_reply_to,"references":message.reference_ids,
        "thread_id":message.thread_id,"subject":message.subject,"from_name":message.from_name,
        "from_address":message.from_address,"to_addresses":message.to_addresses,"cc_addresses":message.cc_addresses,
        "bcc_addresses":message.bcc_addresses,"reply_to_addresses":message.reply_to_addresses,
        "received_at":message.received_at,"is_read":message.is_read,"is_flagged":message.is_flagged,
        "has_attachments":message.has_attachments})
}

// Complete parsing starts with a local identity; catalogue parsing may derive a
// thread root before Store reconciliation. Compare raw threading fields across
// these boundaries and preserve the complete projection exactly across reopen.
fn parse_path_metadata_projection(message: &MailSummary) -> Value {
    let mut projection = metadata_projection(message);
    projection.as_object_mut().unwrap().remove("thread_id");
    projection
}

fn assert_ordered_tokens(value: &str, tokens: &Value, label: &str) {
    let mut after = 0;
    for token in tokens.as_array().expect("declared tokens") {
        let token = token.as_str().expect("string token");
        let offset = value[after..]
            .find(token)
            .unwrap_or_else(|| panic!("{label} lost token {token:?}: {value:?}"));
        after += offset + token.len();
    }
}

fn assert_obsolete_metadata(message: &MailSummary, expected: &Value, label: &str) {
    for (field, actual) in [
        ("subject", message.subject.as_str()),
        ("fromAddress", message.from_address.as_str()),
        ("toAddresses", message.to_addresses.as_str()),
        ("ccAddresses", message.cc_addresses.as_str()),
        ("bccAddresses", message.bcc_addresses.as_str()),
        ("replyToAddresses", message.reply_to_addresses.as_str()),
    ] {
        if let Some(wanted) = expected.get(field).and_then(Value::as_str) {
            assert_eq!(actual, wanted, "{label}: {field}");
        }
    }
    if let Some(wanted) = expected.get("fromName").and_then(Value::as_str) {
        assert_eq!(
            message.from_name.as_deref(),
            Some(wanted),
            "{label}: display name"
        );
    }
    if let Some(allowed) = expected.get("fromAllowed").and_then(Value::as_array) {
        assert!(
            allowed
                .iter()
                .any(|value| value.as_str() == Some(message.from_address.as_str())),
            "{label}: literal quoted local-part identity changed: {:?}",
            message.from_address
        );
    }
    for (field, actual) in [
        ("toAllowed", &message.to_addresses),
        ("ccAllowed", &message.cc_addresses),
    ] {
        if let Some(allowed) = expected.get(field).and_then(Value::as_array) {
            assert!(
                allowed
                    .iter()
                    .any(|value| value.as_str() == Some(actual.as_str())),
                "{label}: {field} invented or removed a recipient: {actual:?}"
            );
        }
    }
    for (field, value) in [
        ("subjectTokens", &message.subject),
        ("toTokens", &message.to_addresses),
        ("fromTokens", &message.from_address),
    ] {
        if let Some(tokens) = expected.get(field) {
            assert_ordered_tokens(value, tokens, label);
        }
    }
    let wanted = chrono::DateTime::parse_from_rfc3339(expected["receivedAt"].as_str().unwrap())
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(
        message.received_at, wanted,
        "{label}: valid obsolete Date must not be hidden by INTERNALDATE fallback"
    );
    for (field, actual) in [
        ("messageId", &message.message_id),
        ("inReplyTo", &message.in_reply_to),
        ("references", &message.reference_ids),
    ] {
        if let Some(wanted) = expected.get(field).and_then(Value::as_str) {
            assert_eq!(actual.as_deref(), Some(wanted), "{label}: {field}");
        }
    }
    assert!(
        !message.has_attachments,
        "{label}: plain historical input invented an attachment"
    );
}

async fn assert_obsolete_case(name: &str, raw: &[u8]) {
    let case = obsolete_spec(name);
    let expected = &case["expected"];
    let uid = case["uid"].as_u64().unwrap() as u32;
    let mut account = test_account();
    account.id = uuid::Uuid::parse_str("53535353-5353-4353-8353-535353535353").unwrap();
    let response = vec![format!(
        "* 1 FETCH (UID {uid} FLAGS () INTERNALDATE \"01-Jan-2001 00:00:00 +0000\")"
    )];
    let mut complete = parse_message(&account, "INBOX", uid, &response, raw, None)
        .await
        .unwrap_or_else(|error| {
            panic!("{name}: complete parser rejected historical receive syntax: {error:#}")
        });
    if name == "obs-06-zone-matrix-a" {
        // RFC 5322 section 4.3 recommends interpreting military letters as
        // -0000, not their unreliable historical offsets. MailSummary stores
        // UTC instants only, so this proves the instant, not unknown-offset
        // provenance, which remains an explicit profile limitation.
        let unknown_offset_raw = std::str::from_utf8(raw)
            .unwrap()
            .replace("12:00:00 A\r\n", "12:00:00 -0000\r\n");
        assert_ne!(unknown_offset_raw.as_bytes(), raw);
        let unknown_offset = parse_message(
            &account,
            "INBOX",
            uid,
            &response,
            unknown_offset_raw.as_bytes(),
            None,
        )
        .await
        .expect("explicit unknown-offset control remains readable");
        assert_eq!(
            complete.received_at, unknown_offset.received_at,
            "military letter must use the existing -0000 instant semantics"
        );
    }
    let headers =
        parse_header_message(&account, "INBOX", uid, &response, raw).unwrap_or_else(|error| {
            panic!("{name}: header parser rejected historical receive syntax: {error:#}")
        });
    let catalogue = parse_catalog_message(
        &account,
        "INBOX",
        uid,
        &response,
        raw,
        "fictional preview".into(),
    )
    .unwrap_or_else(|error| {
        panic!("{name}: catalogue parser rejected historical receive syntax: {error:#}")
    });
    for (path, message) in [
        ("complete", &complete),
        ("header", &headers),
        ("catalogue", &catalogue),
    ] {
        assert_obsolete_metadata(message, expected, &format!("{name}/{path}"));
        assert_eq!(message.account_id, account.id.to_string());
        assert_eq!(message.mailbox, "INBOX");
        assert_eq!(message.uid, i64::from(uid));
    }
    assert_eq!(
        parse_path_metadata_projection(&headers),
        parse_path_metadata_projection(&complete),
        "{name}: header/complete semantic projection"
    );
    assert_eq!(
        parse_path_metadata_projection(&catalogue),
        parse_path_metadata_projection(&complete),
        "{name}: catalogue/complete semantic projection"
    );
    assert!(complete.attachments.is_empty());
    assert!(complete.body_html.is_none());
    if let Some(body) = expected.get("bodyText").and_then(Value::as_str) {
        assert_eq!(complete.body_text, body, "{name}: body bytes before cache");
    }
    if let Some(tokens) = expected.get("bodyTokens") {
        assert_ordered_tokens(&complete.body_text, tokens, name);
    }
    complete.mailbox_uid_validity = Some(OBSOLETE_EPOCH);
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("obsolete-receive.sqlite");
    let store = Store::open(&database).await.unwrap();
    store.save_account(&account).await.unwrap();
    store
        .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", OBSOLETE_EPOCH as u32, 1, true)
        .await
        .unwrap();
    store
        .upsert_messages(std::slice::from_ref(&complete))
        .await
        .unwrap();
    store
        .cache_message_content(
            &complete.id,
            complete.is_flagged,
            CachedMessageContent {
                body_text: complete.body_text.clone(),
                body_html: complete.body_html.clone(),
                unsubscribe_kind: complete.unsubscribe_kind.clone(),
                attachments: Vec::new(),
            },
        )
        .await
        .unwrap();
    // Store deliberately reconciles the provisional thread ID on upsert. Pin
    // its committed projection before close, then prove reopen preserves it.
    let committed = store.message(&complete.id).await.unwrap().unwrap();
    assert_eq!(
        parse_path_metadata_projection(&committed),
        parse_path_metadata_projection(&complete),
        "{name}: Store must retain all received metadata during reconciliation"
    );
    drop(store);
    let reopened = Store::open(&database).await.unwrap();
    let stored = reopened
        .message(&complete.id)
        .await
        .unwrap()
        .expect("historical message survives reopen");
    let cached = reopened
        .cached_message_content(&complete.id)
        .await
        .unwrap()
        .expect("Reader content survives reopen");
    assert_obsolete_metadata(&stored, expected, &format!("{name}/reopen"));
    assert_eq!(
        metadata_projection(&stored),
        metadata_projection(&committed),
        "{name}: all projected metadata survives reopen"
    );
    assert_eq!(stored.mailbox_uid_validity, Some(OBSOLETE_EPOCH));
    let reader_projection = json!({"message": stored, "content": {
        "body_text":cached.body_text,"body_html":cached.body_html,"attachments":cached.attachments,
    }});
    assert_eq!(
        reader_projection["content"]["body_text"].as_str(),
        Some(complete.body_text.as_str()),
        "{name}: cached core Reader payload must not substitute historical body text"
    );
    assert!(reader_projection["content"]["attachments"]
        .as_array()
        .unwrap()
        .is_empty());
}

async fn assert_raw_export_preserves_obsolete_bytes(name: &str, uid: u32, raw: &'static [u8]) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let account = imap_test_account(Security::Tls, port);
    let store = Store::in_memory().await.unwrap();
    store.save_account(&account).await.unwrap();
    store
        .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", OBSOLETE_EPOCH as u32, 1, true)
        .await
        .unwrap();
    let service = MailService::new(store.clone());
    service
        .credentials()
        .set_password(&account, "fictional obsolete export")
        .await
        .unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = smtp_test_acceptor().accept(stream).await.unwrap();
        let (read, mut write) = split(stream);
        let mut read = BufReader::new(read);
        write
            .write_all(b"* OK fictional obsolete export ready\r\n")
            .await
            .unwrap();
        write.flush().await.unwrap();
        let mut transcript = Vec::new();
        loop {
            let mut line = String::new();
            assert_ne!(
                read.read_line(&mut line).await.unwrap(),
                0,
                "client closed before raw export finished"
            );
            let (tag, command) = line.trim_end().split_once(' ').unwrap();
            transcript.push(command.to_owned());
            let reply = match command {
                "CAPABILITY" => format!("* CAPABILITY IMAP4rev1\r\n{tag} OK capability\r\n"),
                "LOGIN \"reader@example.test\" \"fictional obsolete export\"" => format!("{tag} OK login\r\n"),
                "SELECT \"INBOX\"" => format!("* 1 EXISTS\r\n* OK [UIDVALIDITY {OBSOLETE_EPOCH}] stable\r\n{tag} OK select\r\n"),
                "LOGOUT" => { write.write_all(format!("* BYE done\r\n{tag} OK logout\r\n").as_bytes()).await.unwrap();write.flush().await.unwrap();break; },
                _ if command == format!("UID FETCH {uid} (BODY.PEEK[])") => {
                    write.write_all(format!("* 1 FETCH (UID {uid} BODY[] {{{}}}\r\n", raw.len()).as_bytes()).await.unwrap();
                    write.write_all(raw).await.unwrap();
                    format!("\r\n)\r\n{tag} OK literal\r\n")
                },
                _ => panic!("unexpected raw-export command: {command}"),
            };
            write.write_all(reply.as_bytes()).await.unwrap();
            write.flush().await.unwrap();
        }
        transcript
    });
    let exported = tokio::time::timeout(
        Duration::from_secs(5),
        service.fetch_raw_message_at_epoch_with_endpoint_for_test(
            &account,
            "INBOX",
            uid,
            OBSOLETE_EPOCH,
            imap_test_endpoint(port, true),
        ),
    )
    .await
    .expect("bounded fictional raw export")
    .expect("raw historical source remains exportable");
    assert_eq!(
        exported, raw,
        "{name}: raw export changed historical octets"
    );
    let transcript = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert!(transcript.contains(&format!("UID FETCH {uid} (BODY.PEEK[])")));
    assert!(transcript.iter().all(|command| !command.contains("STORE")
        && !command.starts_with("APPEND")
        && !command.contains("EXPUNGE")));
}

#[tokio::test]
async fn obsolete_receive_obs_01_comment_qtext() {
    assert_obsolete_case(
        "obs-01-comment-qtext",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-01-comment-qtext.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_01_unstructured() {
    assert_obsolete_case(
        "obs-01-unstructured",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-01-unstructured.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_02_body_controls() {
    assert_obsolete_case(
        "obs-02-body-controls",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-02-body-controls.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_02_mixed_header_separators() {
    assert_obsolete_case(
        "obs-02-mixed-header-separators",
        include_bytes!(
            "../../../testdata/mime/obsolete-receive/obs-02-mixed-header-separators.eml"
        ),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_03_dotted_display() {
    assert_obsolete_case(
        "obs-03-dotted-display",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-03-dotted-display.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_03_keyword_nulls() {
    assert_obsolete_case(
        "obs-03-keyword-nulls",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-03-keyword-nulls.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_priority_obs_04_whitespace_only_fold() {
    assert_obsolete_case(
        "obs-04-whitespace-only-fold",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-04-whitespace-only-fold.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_priority_obs_05_year_thresholds_00() {
    assert_obsolete_case(
        "obs-05-year-thresholds-00",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-05-year-thresholds-00.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_priority_obs_05_year_thresholds_49() {
    assert_obsolete_case(
        "obs-05-year-thresholds-49",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-05-year-thresholds-49.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_priority_obs_05_year_thresholds_50() {
    assert_obsolete_case(
        "obs-05-year-thresholds-50",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-05-year-thresholds-50.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_priority_obs_05_year_thresholds_99() {
    assert_obsolete_case(
        "obs-05-year-thresholds-99",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-05-year-thresholds-99.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_priority_obs_05_year_thresholds_123() {
    assert_obsolete_case(
        "obs-05-year-thresholds-123",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-05-year-thresholds-123.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_05_date_token_comments() {
    assert_obsolete_case(
        "obs-05-date-token-comments",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-05-date-token-comments.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_06_zone_matrix_ut() {
    assert_obsolete_case(
        "obs-06-zone-matrix-ut",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-06-zone-matrix-ut.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_06_zone_matrix_gmt() {
    assert_obsolete_case(
        "obs-06-zone-matrix-gmt",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-06-zone-matrix-gmt.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_06_zone_matrix_est() {
    assert_obsolete_case(
        "obs-06-zone-matrix-est",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-06-zone-matrix-est.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_06_zone_matrix_edt() {
    assert_obsolete_case(
        "obs-06-zone-matrix-edt",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-06-zone-matrix-edt.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_06_zone_matrix_cst() {
    assert_obsolete_case(
        "obs-06-zone-matrix-cst",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-06-zone-matrix-cst.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_06_zone_matrix_cdt() {
    assert_obsolete_case(
        "obs-06-zone-matrix-cdt",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-06-zone-matrix-cdt.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_06_zone_matrix_mst() {
    assert_obsolete_case(
        "obs-06-zone-matrix-mst",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-06-zone-matrix-mst.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_06_zone_matrix_mdt() {
    assert_obsolete_case(
        "obs-06-zone-matrix-mdt",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-06-zone-matrix-mdt.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_06_zone_matrix_pst() {
    assert_obsolete_case(
        "obs-06-zone-matrix-pst",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-06-zone-matrix-pst.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_06_zone_matrix_pdt() {
    assert_obsolete_case(
        "obs-06-zone-matrix-pdt",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-06-zone-matrix-pdt.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_06_zone_matrix_a() {
    assert_obsolete_case(
        "obs-06-zone-matrix-a",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-06-zone-matrix-a.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_priority_obs_07_direct_route() {
    assert_obsolete_case(
        "obs-07-direct-route",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-07-direct-route.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_08_cfws_dot() {
    assert_obsolete_case(
        "obs-08-cfws-dot",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-08-cfws-dot.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_08_quoted_word_dot() {
    assert_obsolete_case(
        "obs-08-quoted-word-dot",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-08-quoted-word-dot.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_09_null_recipients() {
    assert_obsolete_case(
        "obs-09-null-recipients",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-09-null-recipients.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_10_domain_escape() {
    assert_obsolete_case(
        "obs-10-domain-escape",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-10-domain-escape.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_10_domain_control() {
    assert_obsolete_case(
        "obs-10-domain-control",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-10-domain-control.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_11_reorder_and_wsp() {
    assert_obsolete_case(
        "obs-11-reorder-and-wsp",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-11-reorder-and-wsp.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_12_date_wsp() {
    assert_obsolete_case(
        "obs-12-date-wsp",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-12-date-wsp.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_13_originator_fields() {
    assert_obsolete_case(
        "obs-13-originator-fields",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-13-originator-fields.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_priority_obs_14_repeat_destinations() {
    assert_obsolete_case(
        "obs-14-repeat-destinations",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-14-repeat-destinations.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_priority_obs_15_thread_phrases() {
    assert_obsolete_case(
        "obs-15-thread-phrases",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-15-thread-phrases.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_priority_obs_15_thread_cfws() {
    assert_obsolete_case(
        "obs-15-thread-cfws",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-15-thread-cfws.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_16_info_wsp() {
    assert_obsolete_case(
        "obs-16-info-wsp",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-16-info-wsp.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_17_resent_block() {
    assert_obsolete_case(
        "obs-17-resent-block",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-17-resent-block.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_18_trace_wsp() {
    assert_obsolete_case(
        "obs-18-trace-wsp",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-18-trace-wsp.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_obs_19_optional_wsp() {
    assert_obsolete_case(
        "obs-19-optional-wsp",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-19-optional-wsp.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_adjacent_quoted_local_identity_stays_literal() {
    assert_obsolete_case(
        "quoted-local-encoded-looking-identity",
        include_bytes!(
            "../../../testdata/mime/obsolete-receive/quoted-local-encoded-looking-identity.eml"
        ),
    )
    .await;
}

#[tokio::test]
async fn obsolete_unexposed_source_fields_export_exact_octets_over_production_tls() {
    assert_raw_export_preserves_obsolete_bytes(
        "obs-01-comment-qtext",
        1200,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-01-comment-qtext.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-01-unstructured",
        1201,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-01-unstructured.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-02-body-controls",
        1202,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-02-body-controls.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-02-mixed-header-separators",
        1203,
        include_bytes!(
            "../../../testdata/mime/obsolete-receive/obs-02-mixed-header-separators.eml"
        ),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-03-dotted-display",
        1204,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-03-dotted-display.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-03-keyword-nulls",
        1205,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-03-keyword-nulls.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-04-whitespace-only-fold",
        1206,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-04-whitespace-only-fold.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-05-year-thresholds-00",
        1207,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-05-year-thresholds-00.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-05-year-thresholds-49",
        1208,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-05-year-thresholds-49.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-05-year-thresholds-50",
        1209,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-05-year-thresholds-50.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-05-year-thresholds-99",
        1210,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-05-year-thresholds-99.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-05-year-thresholds-123",
        1211,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-05-year-thresholds-123.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-05-date-token-comments",
        1212,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-05-date-token-comments.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-06-zone-matrix-ut",
        1213,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-06-zone-matrix-ut.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-06-zone-matrix-gmt",
        1214,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-06-zone-matrix-gmt.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-06-zone-matrix-est",
        1215,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-06-zone-matrix-est.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-06-zone-matrix-edt",
        1216,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-06-zone-matrix-edt.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-06-zone-matrix-cst",
        1217,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-06-zone-matrix-cst.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-06-zone-matrix-cdt",
        1218,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-06-zone-matrix-cdt.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-06-zone-matrix-mst",
        1219,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-06-zone-matrix-mst.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-06-zone-matrix-mdt",
        1220,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-06-zone-matrix-mdt.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-06-zone-matrix-pst",
        1221,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-06-zone-matrix-pst.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-06-zone-matrix-pdt",
        1222,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-06-zone-matrix-pdt.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-06-zone-matrix-a",
        1223,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-06-zone-matrix-a.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-07-direct-route",
        1224,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-07-direct-route.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-08-cfws-dot",
        1225,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-08-cfws-dot.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-08-quoted-word-dot",
        1226,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-08-quoted-word-dot.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-09-null-recipients",
        1227,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-09-null-recipients.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-10-domain-escape",
        1228,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-10-domain-escape.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-10-domain-control",
        1229,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-10-domain-control.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-11-reorder-and-wsp",
        1230,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-11-reorder-and-wsp.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-12-date-wsp",
        1231,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-12-date-wsp.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-13-originator-fields",
        1232,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-13-originator-fields.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-14-repeat-destinations",
        1233,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-14-repeat-destinations.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-15-thread-phrases",
        1234,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-15-thread-phrases.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-15-thread-cfws",
        1235,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-15-thread-cfws.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-16-info-wsp",
        1236,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-16-info-wsp.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-17-resent-block",
        1237,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-17-resent-block.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-18-trace-wsp",
        1238,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-18-trace-wsp.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-19-optional-wsp",
        1239,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-19-optional-wsp.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "quoted-local-encoded-looking-identity",
        1240,
        include_bytes!(
            "../../../testdata/mime/obsolete-receive/quoted-local-encoded-looking-identity.eml"
        ),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-route-control-group",
        1300,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-route-control-group.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-route-control-null-relays",
        1301,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-route-control-null-relays.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-route-control-invalid-domain",
        1302,
        include_bytes!(
            "../../../testdata/mime/obsolete-receive/obs-route-control-invalid-domain.eml"
        ),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-route-control-colon-lookalike",
        1303,
        include_bytes!(
            "../../../testdata/mime/obsolete-receive/obs-route-control-colon-lookalike.eml"
        ),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-route-control-quoted-local",
        1304,
        include_bytes!(
            "../../../testdata/mime/obsolete-receive/obs-route-control-quoted-local.eml"
        ),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-route-control-comments",
        1305,
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-route-control-comments.eml"),
    )
    .await;
    assert_raw_export_preserves_obsolete_bytes(
        "obs-route-control-encoded-local",
        1306,
        include_bytes!(
            "../../../testdata/mime/obsolete-receive/obs-route-control-encoded-local.eml"
        ),
    )
    .await;
}

#[tokio::test]
async fn obsolete_receive_identity_regenerates_modern_reply_headers_without_copying_source_syntax()
{
    let mut account = test_account();
    account.email = "reader@example.test".into();
    let cases: [(&[u8], &str, Option<&str>); 2] = [
        (
            include_bytes!("../../../testdata/mime/obsolete-receive/obs-07-direct-route.eml"),
            "sender@example.test",
            Some("<obs-obs-07-direct-route@example.test>"),
        ),
        (
            include_bytes!("../../../testdata/mime/obsolete-receive/obs-15-thread-cfws.eml"),
            "sender@example.test",
            Some("<left.part@example.test>"),
        ),
    ];
    for (raw, expected_recipient, expected_id) in cases {
        let received = parse_message(&account, "INBOX", 9001, &[], raw, None)
            .await
            .expect("valid historical source must be readable before composing");
        assert_eq!(received.from_address, expected_recipient);
        assert_eq!(received.message_id.as_deref(), expected_id);
        // This is the production MIME builder with explicitly selected received
        // identity fields, not a claim about the frontend Reply action.
        let draft = ComposeMessage {
            account_id: account.id,
            to: vec![received.from_address],
            cc: vec![],
            bcc: vec![],
            subject: "Fictional reply to obsolete input".into(),
            body_text: "Fictional reply body.".into(),
            body_html: None,
            in_reply_to: received.message_id.clone(),
            references: received.message_id,
            attachments: vec![],
        };
        let wire = build_compose_message(&account, &draft).unwrap().formatted();
        let header = std::str::from_utf8(&wire)
            .unwrap()
            .split_once("\r\n\r\n")
            .unwrap()
            .0;
        assert!(header.contains("To: sender@example.test\r\n"));
        assert!(header.contains(&format!("In-Reply-To: {}\r\n", expected_id.unwrap())));
        assert!(!header.contains("@relay-a.example"));
        assert!(!header.contains("(note)"));
        for line in header
            .split("\r\n")
            .filter(|line| !line.starts_with([' ', '\t']))
        {
            let (name, _) = line.split_once(':').expect("generated field has colon");
            assert!(
                !name.ends_with([' ', '\t']),
                "obsolete pre-colon whitespace: {line:?}"
            );
        }
        let date_field = header
            .split("\r\n")
            .find_map(|line| line.strip_prefix("Date: "))
            .expect("one modern generated Date field");
        let date_tokens: Vec<_> = date_field.split_ascii_whitespace().collect();
        assert_eq!(date_tokens.len(), 6, "modern Date grammar: {date_field:?}");
        assert_eq!(
            date_tokens[3].len(),
            4,
            "generator must emit four year digits"
        );
        assert!(date_tokens[3].bytes().all(|byte| byte.is_ascii_digit()));
        assert_eq!(
            date_tokens[5].len(),
            5,
            "generator must emit a numeric zone"
        );
        assert!(date_tokens[5].starts_with(['+', '-']));
        assert!(date_tokens[5].as_bytes()[1..]
            .iter()
            .all(u8::is_ascii_digit));
        let generated = mail_parser::MessageParser::default().parse(&wire).unwrap();
        let date = generated.date().expect("new message has Date");
        assert!(
            date.year >= 2000,
            "generator must emit a modern four-digit year"
        );
    }
}

#[tokio::test]
async fn obsolete_route_control_group() {
    assert_obsolete_case(
        "obs-route-control-group",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-route-control-group.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_route_control_null_relays() {
    assert_obsolete_case(
        "obs-route-control-null-relays",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-route-control-null-relays.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_route_control_invalid_domain() {
    assert_obsolete_case(
        "obs-route-control-invalid-domain",
        include_bytes!(
            "../../../testdata/mime/obsolete-receive/obs-route-control-invalid-domain.eml"
        ),
    )
    .await;
}

#[tokio::test]
async fn obsolete_route_control_colon_lookalike() {
    assert_obsolete_case(
        "obs-route-control-colon-lookalike",
        include_bytes!(
            "../../../testdata/mime/obsolete-receive/obs-route-control-colon-lookalike.eml"
        ),
    )
    .await;
}

#[tokio::test]
async fn obsolete_route_control_quoted_local() {
    assert_obsolete_case(
        "obs-route-control-quoted-local",
        include_bytes!(
            "../../../testdata/mime/obsolete-receive/obs-route-control-quoted-local.eml"
        ),
    )
    .await;
}

#[tokio::test]
async fn obsolete_route_control_comments() {
    assert_obsolete_case(
        "obs-route-control-comments",
        include_bytes!("../../../testdata/mime/obsolete-receive/obs-route-control-comments.eml"),
    )
    .await;
}

#[tokio::test]
async fn obsolete_route_control_encoded_local() {
    assert_obsolete_case(
        "obs-route-control-encoded-local",
        include_bytes!(
            "../../../testdata/mime/obsolete-receive/obs-route-control-encoded-local.eml"
        ),
    )
    .await;
}

#[tokio::test]
async fn obsolete_control_bearing_reply_target_is_rejected_before_smtp_without_cleaning_identity() {
    let raw = include_bytes!("../../../testdata/mime/obsolete-receive/obs-10-domain-control.eml");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let account = smtp_test_account(Security::Tls, port);
    let received = parse_message(&account, "INBOX", 1229, &[], raw, None)
        .await
        .expect("control-bearing historical source stays readable");
    assert_eq!(received.from_address, "sender@[192.0.2.10\u{1}]");
    let draft = ComposeMessage {
        account_id: account.id,
        to: vec![received.from_address],
        cc: vec![],
        bcc: vec![],
        subject: "Fictional control-target reply".into(),
        body_text: "This test must never submit a message.".into(),
        body_html: None,
        in_reply_to: received.message_id,
        references: None,
        attachments: vec![],
    };
    let validation_error = build_compose_message(&account, &draft)
        .expect_err("builder must reject the exact received target before any SMTP boundary");
    assert!(
        format!("{validation_error:#}").contains("recipient contains an invalid header character"),
        "builder must reject the received identity for its control character: {validation_error:#}"
    );
    let service = MailService::new(Store::in_memory().await.unwrap());
    let error = service
        .send_with_smtp_endpoint(
            &account,
            &draft,
            "fictional secret",
            smtp_test_endpoint(&account),
            Duration::from_millis(500),
        )
        .await
        .expect_err("control-bearing target must not be sanitized into another mailbox");
    // Public send errors deliberately redact validation internals; the exact
    // builder cause above and typed production outcome below are separate.

    assert_eq!(
        error.downcast_ref::<crate::SendFailure>().unwrap().kind,
        crate::SendFailureKind::NotSubmitted
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(200), listener.accept())
            .await
            .is_err(),
        "control-bearing received identity opened an SMTP connection"
    );
    // Positive control for the exact endpoint, CA and transport profile above:
    // after invalid input opens no socket, a legitimate auth-only operation
    // reaches this same loopback server and stops at QUIT without submission.
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let stream = smtp_test_acceptor().accept(socket).await.unwrap();
        let mut connection = BufReader::new(stream);
        smtp_reply(&mut connection, "220 fictional control ready\r\n").await;
        smtp_expect_ehlo_and_auth(&mut connection, false).await;
        assert_eq!(smtp_command(&mut connection).await, "QUIT\r\n");
        smtp_reply(&mut connection, "221 done\r\n").await;
    });
    smtp_auth_probe_with_smtp_endpoint(
        &account,
        "secret",
        smtp_test_endpoint(&account),
        Duration::from_secs(5),
    )
    .await
    .expect("same SMTP endpoint and trust configuration must be usable");
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
}
