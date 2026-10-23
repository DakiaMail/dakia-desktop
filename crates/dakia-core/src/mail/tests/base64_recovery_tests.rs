//! RFC 2045 section 6.8 receive policy at complete parse and MailService TLS boundaries.
//!
//! Inputs extract the first ASCII/base64 leaf and original envelope headers from
//! the governed, synthetic `standards-base64-final-quanta.eml` corpus fixture.
//! Only that leaf's transfer bytes are changed in memory. No private mail is used.
//! RFC source: https://www.rfc-editor.org/rfc/rfc2045.html#section-6.8
//! Invalid nonalphabet bytes may warrant rejection; incomplete quanta must never
//! silently publish a decoded prefix as though the complete body had been read.

use super::*;
use tokio::{
    io::{split, AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    task::JoinHandle,
};

const BASE64_UID_VALIDITY: i64 = 8_064;

fn derived_base64_message(body: &[u8]) -> Vec<u8> {
    let fixture = include_str!("../../../testdata/mime/standards-base64-final-quanta.eml");
    let envelope = fixture
        .split_once("MIME-Version:")
        .expect("governed envelope")
        .0;
    let first_leaf = fixture
        .split("--base64-quanta\n")
        .nth(1)
        .expect("first governed leaf");
    let (leaf_headers, original_body) = first_leaf
        .split_once("\n\n")
        .expect("governed leaf separator");
    assert!(
        original_body.starts_with("QQ==\n"),
        "derive from the canonical one-byte final quantum"
    );
    let mut raw = format!("{envelope}MIME-Version: 1.0\n{leaf_headers}\n\n")
        .replace('\n', "\r\n")
        .into_bytes();
    raw.extend_from_slice(body);
    raw
}

fn base64_account() -> Account {
    let mut account = imap_test_account(Security::Tls, 993);
    account.id = uuid::Uuid::parse_str("62626262-6262-4262-8262-626262626262")
        .expect("fixed fictional account UUID");
    account.email = "reader@example.test".into();
    account.auth = AccountAuth::Password {
        username: "reader@example.test".into(),
    };
    account
}

async fn seed_base64_catalogue(store: &Store, account: &Account, uids: &[u32]) {
    store
        .save_mailbox_catalog_state(
            account.id,
            "INBOX",
            "INBOX",
            BASE64_UID_VALIDITY as u32,
            uids.len(),
            true,
        )
        .await
        .expect("save fictional mailbox state");
    for uid in uids {
        let headers = format!(
            "Date: Tue, 28 Jul 2026 12:00:00 +0000\r\nFrom: Sender <sender@example.test>\r\nTo: Reader <reader@example.test>\r\nSubject: MIME recovery fixture {uid}\r\nMessage-ID: <mime-recovery-{uid}@example.test>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n"
        );
        let mut message = parse_catalog_message(
            account,
            "INBOX",
            *uid,
            &[],
            headers.as_bytes(),
            "catalogue preview".into(),
        )
        .expect("parse fictional catalogue row");
        message.mailbox_uid_validity = Some(BASE64_UID_VALIDITY);
        store
            .upsert_messages(std::slice::from_ref(&message))
            .await
            .expect("save fictional catalogue row");
    }
}

async fn base64_hydration_server(
    uid: u32,
    raw: Vec<u8>,
) -> (ImapEndpoint, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fictional MIME IMAP listener");
    let port = listener.local_addr().expect("listener address").port();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept fictional client");
        let stream = smtp_test_acceptor()
            .accept(stream)
            .await
            .expect("accept fictional TLS");
        let (read, mut write) = split(stream);
        let mut read = BufReader::new(read);
        let separator = raw
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .expect("derived single-part MIME separator")
            + 4;
        let (headers, body) = raw.split_at(separator);
        let transfer_encoding = "base64";
        let advertised_size = body.len();
        let mut transcript = Vec::new();
        write
            .write_all(b"* OK fictional MIME IMAP ready\r\n")
            .await
            .expect("write greeting");
        loop {
            let mut line = String::new();
            match read.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::UnexpectedEof
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                    ) =>
                {
                    break;
                }
                Err(error) => panic!("read fictional command: {error}"),
            }
            let (tag, command) = line
                .trim_end()
                .split_once(' ')
                .expect("tagged fictional command");
            transcript.push(command.to_owned());
            if command == "CAPABILITY" {
                write
                    .write_all(
                        format!("* CAPABILITY IMAP4rev1\r\n{tag} OK capability\r\n").as_bytes(),
                    )
                    .await
                    .expect("write capability");
            } else if command == "LOGIN \"reader@example.test\" \"base64 secret\"" {
                write
                    .write_all(format!("{tag} OK authenticated\r\n").as_bytes())
                    .await
                    .expect("write authentication");
            } else if command == "SELECT \"INBOX\"" {
                write
                    .write_all(
                        format!(
                            "* 4 EXISTS\r\n* OK [UIDVALIDITY {BASE64_UID_VALIDITY}] fictional epoch\r\n{tag} OK selected\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write selection");
            } else if command.starts_with(&format!(
                "UID FETCH {uid} (FLAGS INTERNALDATE BODYSTRUCTURE"
            )) {
                write
                    .write_all(
                        format!(
                            "* 1 FETCH (UID {uid} FLAGS () INTERNALDATE \"28-Jul-2026 12:00:00 +0000\" BODYSTRUCTURE (\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"{transfer_encoding}\" {} 1) BODY[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES CONTENT-TYPE CONTENT-TRANSFER-ENCODING CONTENT-ID CONTENT-LOCATION CONTENT-DISPOSITION LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST PRECEDENCE AUTO-SUBMITTED)] {{{}}}\r\n",
                            advertised_size,
                            headers.len(),
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write metadata prefix");
                write
                    .write_all(headers)
                    .await
                    .expect("write metadata literal");
                write
                    .write_all(format!(")\r\n{tag} OK metadata\r\n").as_bytes())
                    .await
                    .expect("finish metadata");
            } else if command == format!("UID FETCH {uid} (BODY.PEEK[TEXT])") {
                write
                    .write_all(
                        format!("* 1 FETCH (UID {uid} BODY[TEXT] {{{}}}\r\n", body.len())
                            .as_bytes(),
                    )
                    .await
                    .expect("write body prefix");
                write.write_all(body).await.expect("write body literal");
                write
                    .write_all(format!("\r\n)\r\n{tag} OK body\r\n").as_bytes())
                    .await
                    .expect("finish body");
            } else if command == "LOGOUT" {
                write
                    .write_all(format!("* BYE done\r\n{tag} OK logout\r\n").as_bytes())
                    .await
                    .expect("write logout");
                write.flush().await.expect("flush logout");
                break;
            } else {
                panic!("unexpected fictional MIME command: {command}");
            }
            write.flush().await.expect("flush fictional response");
        }
        transcript
    });
    (imap_test_endpoint(port, true), server)
}

async fn hydrate_base64_case(
    service: &MailService,
    account: &Account,
    uid: u32,
    raw: Vec<u8>,
) -> Result<MailSummary> {
    let (endpoint, server) = base64_hydration_server(uid, raw).await;
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        service.hydrate_message_with_imap_endpoint_for_test(
            account,
            "INBOX",
            uid,
            BASE64_UID_VALIDITY,
            endpoint,
        ),
    )
    .await
    .expect("bounded fictional TLS hydration");
    let transcript = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("bounded fictional server cleanup")
        .expect("fictional TLS server");
    assert!(
        transcript.contains(&format!("UID FETCH {uid} (BODY.PEEK[TEXT])")),
        "exercise read-neutral production body fetch"
    );
    result
}

fn cache_snapshot(content: Option<crate::CachedMessageContent>) -> Option<serde_json::Value> {
    content.map(|content| {
        serde_json::json!({
            "body_text": content.body_text,
            "body_html": content.body_html,
            "unsubscribe_kind": content.unsubscribe_kind,
            "attachments": content.attachments,
        })
    })
}

async fn assert_base64_receive_policy(body: &[u8], expected: Option<&str>) {
    let directory = tempfile::tempdir().expect("isolated fictional profile");
    let database = directory.path().join("base64-recovery.sqlite");
    let store = Store::open(&database)
        .await
        .expect("open fictional profile");
    let account = base64_account();
    store
        .save_account(&account)
        .await
        .expect("save fictional account");
    seed_base64_catalogue(&store, &account, &[801, 802, 803]).await;
    let service = MailService::new(store.clone());
    service
        .credentials()
        .set_password(&account, "base64 secret")
        .await
        .expect("save fictional credential");

    let first = hydrate_base64_case(&service, &account, 801, derived_base64_message(b"QQ=="))
        .await
        .expect("canonical preceding neighbor hydrates");
    let first_summary = serde_json::to_value(store.message(&first.id).await.unwrap()).unwrap();
    let first_cache = cache_snapshot(store.cached_message_content(&first.id).await.unwrap());
    let target_id = stable_message_id(account.id, "INBOX", 802);
    let target_summary = serde_json::to_value(store.message(&target_id).await.unwrap()).unwrap();
    let complete = parse_message(
        &account,
        "INBOX",
        802,
        &[],
        &derived_base64_message(body),
        None,
    )
    .await;
    let selective =
        hydrate_base64_case(&service, &account, 802, derived_base64_message(body)).await;
    let later = hydrate_base64_case(&service, &account, 803, derived_base64_message(b"QUI="))
        .await
        .expect("canonical later neighbor hydrates after malformed input");
    let later_summary = serde_json::to_value(store.message(&later.id).await.unwrap()).unwrap();
    let later_cache = cache_snapshot(store.cached_message_content(&later.id).await.unwrap());
    let cached = store
        .cached_message_content(&target_id)
        .await
        .expect("read target cache");
    drop(service);
    drop(store);
    let reopened = Store::open(&database)
        .await
        .expect("reopen fictional profile");
    assert_eq!(
        reopened
            .cached_message_content(&first.id)
            .await
            .unwrap()
            .unwrap()
            .body_text,
        "A",
        "earlier valid neighbor remains exact after reopen"
    );
    assert_eq!(
        reopened
            .cached_message_content(&later.id)
            .await
            .unwrap()
            .unwrap()
            .body_text,
        "AB",
        "later valid neighbor remains exact after reopen"
    );
    assert!(
        reopened.message(&target_id).await.unwrap().is_some(),
        "rejected body must not delete recoverable catalogue metadata"
    );
    assert_eq!(
        serde_json::to_value(reopened.message(&first.id).await.unwrap()).unwrap(),
        first_summary,
        "all earlier neighbor summary fields survive the failing fetch and reopen"
    );
    assert_eq!(
        cache_snapshot(reopened.cached_message_content(&first.id).await.unwrap()),
        first_cache,
        "all earlier neighbor content fields survive the failing fetch and reopen"
    );
    assert_eq!(
        serde_json::to_value(reopened.message(&later.id).await.unwrap()).unwrap(),
        later_summary,
        "all later neighbor summary fields survive reopen"
    );
    assert_eq!(
        cache_snapshot(reopened.cached_message_content(&later.id).await.unwrap()),
        later_cache,
        "all later neighbor content fields survive reopen"
    );
    let reopened_target = reopened.cached_message_content(&target_id).await.unwrap();
    match expected {
        None => {
            assert_eq!(
                serde_json::to_value(reopened.message(&target_id).await.unwrap()).unwrap(),
                target_summary,
                "rejected transfer leaves every recoverable catalogue field unchanged"
            );
            assert!(
                cached.is_none(),
                "malformed Base64 must not publish a partial Reader cache: {body:?}"
            );
            assert!(
                reopened_target.is_none(),
                "malformed Base64 cache remains absent after reopen"
            );
            assert_eq!(
                complete
                    .expect_err("complete parse must reject malformed Base64")
                    .to_string(),
                MIME_CONTENT_UNDECODABLE
            );
            assert_eq!(
                selective
                    .expect_err("production TLS hydration must reject malformed Base64")
                    .to_string(),
                MIME_CONTENT_UNDECODABLE
            );
        }
        Some(expected_text) => {
            let complete = complete.expect("documented exact-byte tolerance at complete parse");
            let selective =
                selective.expect("same exact-byte tolerance at production TLS hydration");
            assert_eq!(complete.body_text, expected_text);
            assert_eq!(
                cached.expect("accepted content is cached").body_text,
                complete.body_text,
                "complete and production TLS selective decoding must publish the same exact text"
            );
            assert_eq!(
                reopened_target
                    .expect("accepted content survives reopen")
                    .body_text,
                expected_text
            );
            assert_eq!(selective.id, target_id);
        }
    }
}

#[tokio::test]
async fn base64_invalid_character_is_bounded_rejection_without_partial_cache() {
    // RFC 2045 permits rejecting suspected transmission errors. This is Dakia's
    // chosen policy, not a claim that nonalphabet bytes must always be rejected.
    assert_base64_receive_policy(b"Q@Q==", None).await;
}

#[tokio::test]
async fn base64_truncated_final_quartet_never_publishes_only_the_complete_prefix() {
    assert_base64_receive_policy(b"Q", None).await;
    assert_base64_receive_policy(b"QUJDQ", None).await;
}

#[tokio::test]
async fn base64_missing_padding_never_silently_discards_a_final_octet() {
    assert_base64_receive_policy(b"QQ", None).await;
    assert_base64_receive_policy(b"QQ=", None).await;
    assert_base64_receive_policy(b"QUI", None).await;
    assert_base64_receive_policy(b"QUJDQQ", None).await;
    assert_base64_receive_policy(b"QUJDQUI", None).await;
}

#[tokio::test]
async fn base64_excess_terminal_padding_is_an_explicit_exact_byte_tolerance() {
    // Extra '=' is not canonical RFC 2045 output. Dakia's existing receive
    // tolerance is acceptable only if full and selective bytes remain exact.
    assert_base64_receive_policy(b"QQ===", Some("A")).await;
    assert_base64_receive_policy(b"QUI==", Some("AB")).await;
}

#[tokio::test]
async fn base64_misplaced_padding_never_invents_or_joins_decoded_octets() {
    assert_base64_receive_policy(b"Q===", None).await;
    assert_base64_receive_policy(b"=QQ==", None).await;
    assert_base64_receive_policy(b"QQ==QQ==", None).await;
}
