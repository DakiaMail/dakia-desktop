//! LIST response literal and `\Noselect` protocol-boundary regressions.

use super::*;
use crate::imap_mailbox::encode_modified_utf7;
use tokio::{
    io::{split, AsyncBufReadExt, AsyncWriteExt, BufReader},
    time::{timeout, Duration},
};

async fn accept_list_test_client(
    listener: TcpListener,
    username: &str,
    password: &str,
) -> BufReader<tokio_rustls::server::TlsStream<TcpStream>> {
    let (stream, _) = listener
        .accept()
        .await
        .expect("accept fictional IMAP client");
    let stream = smtp_test_acceptor()
        .accept(stream)
        .await
        .expect("accept fictional IMAP TLS");
    let mut connection = BufReader::new(stream);
    smtp_reply(&mut connection, "* OK fictional IMAP ready\r\n").await;
    assert_eq!(smtp_command(&mut connection).await, "D0001 CAPABILITY\r\n");
    smtp_reply(
        &mut connection,
        "* CAPABILITY IMAP4rev1 MOVE UIDPLUS\r\nD0001 OK capability\r\n",
    )
    .await;
    assert_eq!(
        smtp_command(&mut connection).await,
        format!(
            "D0002 LOGIN {} {}\r\n",
            quote_imap(username),
            quote_imap(password)
        )
    );
    smtp_reply(&mut connection, "D0002 OK authenticated\r\n").await;
    connection
}

#[test]
fn list_literals_bind_only_to_their_own_list_line_and_preserve_delimiter() {
    let wire = "&ZeVnLIqe-";
    let response = ImapResponse {
        lines: vec![
            format!(
                "* LIST (\\HasNoChildren \\Archive) \"/\" {{{}}}\r\n",
                wire.len()
            ),
            "* 9 FETCH (UID 9 BODY[] {4}\r\n".into(),
            "D0001 OK listed\r\n".into(),
        ],
        literals: vec![
            ImapLiteral {
                uid: None,
                data_item: ImapLiteralItem::ListResponse(0),
                bytes: wire.as_bytes().to_vec(),
            },
            ImapLiteral {
                uid: Some(9),
                data_item: ImapLiteralItem::Body("BODY[]".into()),
                bytes: b"body".to_vec(),
            },
        ],
    };

    assert_eq!(literal_length(&response.lines[0]), Some(wire.len()));
    let direct = parse_list_response_line(&response.lines[0], &[&response.literals[0]]);
    assert!(
        direct.is_ok(),
        "unexpected direct parser result: {direct:?}"
    );
    let listing = parse_list_response(&response).expect("parse line-bound LIST literal");
    assert_eq!(listing.len(), 1);
    assert_eq!(listing[0].flags, ["\\hasnochildren", "\\archive"]);
    assert_eq!(listing[0].hierarchy_delimiter.as_deref(), Some("/"));
    assert_eq!(listing[0].remote_name, wire);
    assert!(listing[0].is_selectable());
    assert_eq!(
        decode_modified_utf7(&listing[0].remote_name).expect("decode literal wire name"),
        "日本語"
    );

    let cross_line = ImapResponse {
        lines: response.lines,
        literals: vec![ImapLiteral {
            uid: Some(9),
            data_item: ImapLiteralItem::Body("BODY[]".into()),
            bytes: wire.as_bytes().to_vec(),
        }],
    };
    assert!(parse_list_response(&cross_line).is_err());
}

#[test]
fn list_literal_parser_rejects_missing_extra_noncanonical_and_nonselect_targets() {
    let missing = ImapResponse {
        lines: vec!["* LIST () \"/\" {5}\r\n".into()],
        literals: Vec::new(),
    };
    assert!(parse_list_response(&missing).is_err());

    let noncanonical = ImapResponse {
        lines: vec!["* LIST () \"/\" {5}\r\n".into()],
        literals: vec![ImapLiteral {
            uid: None,
            data_item: ImapLiteralItem::ListResponse(0),
            bytes: b"&AEE-".to_vec(),
        }],
    };
    assert!(parse_list_response(&noncanonical).is_err());

    let extra = ImapResponse {
        lines: vec!["* LIST () \"/\" {5}\r\n".into()],
        literals: vec![
            ImapLiteral {
                uid: None,
                data_item: ImapLiteralItem::ListResponse(0),
                bytes: b"Inbox".to_vec(),
            },
            ImapLiteral {
                uid: None,
                data_item: ImapLiteralItem::ListResponse(0),
                bytes: b"extra".to_vec(),
            },
        ],
    };
    assert!(parse_list_response(&extra).is_err());

    for delimiter in ["\"ab\"", "/", "{1}", "\"\\a\"", "\"界\""] {
        let response = ImapResponse {
            lines: vec![format!("* LIST () {delimiter} \"Inbox\"\r\n")],
            literals: Vec::new(),
        };
        assert!(
            parse_list_response(&response).is_err(),
            "invalid hierarchy delimiter {delimiter:?} must fail closed"
        );
    }
    let control = ImapResponse {
        lines: vec![format!("* LIST () \"{}\" \"Inbox\"\r\n", '\u{1}')],
        literals: Vec::new(),
    };
    assert!(parse_list_response(&control).is_err());

    for (delimiter, expected) in [("\"\\\\\"", "\\"), ("\"\\\"\"", "\"")] {
        let response = ImapResponse {
            lines: vec![format!("* LIST () {delimiter} \"Inbox\"\r\n")],
            literals: Vec::new(),
        };
        assert_eq!(
            parse_list_response(&response)
                .expect("valid escaped delimiter")
                .first()
                .and_then(|mailbox| mailbox.hierarchy_delimiter.as_deref()),
            Some(expected)
        );
    }

    let unknown_attribute = parse_list_response(&ImapResponse {
        lines: vec!["* LIST (\\XVendorFeature \\HasChildren) \".\" \"Projects\"\r\n".into()],
        literals: Vec::new(),
    })
    .expect("unknown LIST attributes are tolerated")
    .pop()
    .expect("one mailbox");
    assert_eq!(
        unknown_attribute.flags,
        ["\\xvendorfeature", "\\haschildren"]
    );
    assert!(unknown_attribute.is_selectable());
    assert_eq!(unknown_attribute.hierarchy_delimiter.as_deref(), Some("."));

    let unknown_with_noselect = parse_list_response(&ImapResponse {
        lines: vec!["* LIST (\\XVendorFeature \\NoSeLeCt) \".\" \"Projects\"\r\n".into()],
        literals: Vec::new(),
    })
    .expect("NoSelect response parses")
    .pop()
    .expect("one mailbox");
    assert!(!unknown_with_noselect.is_selectable());
    assert!(
        listed_mailbox_is_selectable(std::slice::from_ref(&unknown_with_noselect), "Projects")
            .is_err()
    );

    let noselect = ListedMailbox {
        flags: vec!["\\noselect".into(), "\\sent".into()],
        hierarchy_delimiter: Some("/".into()),
        remote_name: "Sent".into(),
    };
    let account = test_account();
    assert!(sent_mailbox_from_listed_mailboxes(&account, std::slice::from_ref(&noselect)).is_err());
    assert!(action_destination_from_listed_mailboxes(
        &account,
        MailboxAction::Trash,
        &[ListedMailbox {
            flags: vec!["\\noselect".into(), "\\trash".into()],
            hierarchy_delimiter: Some("/".into()),
            remote_name: "Trash".into(),
        }],
    )
    .is_err());
    assert!(resolve_special_mailboxes_from_listing(
        vec![MailboxPlan::new("Sent", "Sent")],
        &[noselect],
    )
    .iter()
    .any(|plan| plan.remote == "Sent"));
    assert_eq!(
        sent_mailbox_from_listed_mailboxes(&account, &[]).expect("LIST absence keeps fallback"),
        "Sent"
    );
}

#[tokio::test]
async fn list_literal_limit_rejects_before_reading_the_declared_mailbox_bytes() {
    let (mut client, server) = duplex_imap_client_and_server();
    let server = tokio::spawn(async move {
        let (read, mut write) = split(server);
        let mut read = BufReader::new(read);
        let mut command = String::new();
        assert_ne!(
            read.read_line(&mut command)
                .await
                .expect("read LIST command"),
            0
        );
        assert_eq!(command, "D0001 LIST \"\" \"*\"\r\n");
        write
            .write_all(
                format!(
                    "* LIST (\\HasNoChildren \\Archive) \"/\" {{{}}}\r\n",
                    MAX_IMAP_LIST_LITERAL_BYTES + 1
                )
                .as_bytes(),
            )
            .await
            .expect("write oversized LIST marker only");
        write.flush().await.expect("flush oversized LIST marker");
    });

    let error = client
        .list_mailboxes()
        .await
        .expect_err("oversized LIST literals must fail before allocation");
    assert!(error.to_string().contains("per-message safety limit"));
    server.await.expect("fictional IMAP server completed");
}

#[tokio::test]
async fn tls_list_literal_sync_selects_and_reopens_the_exact_wire_identity() {
    let profile = tempfile::tempdir().expect("temporary fictional profile");
    let database = profile.path().join("literal-list-sync.sqlite");
    let store = Store::open(&database)
        .await
        .expect("open fictional profile");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fictional IMAP server");
    let mut account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
    account.archive_mailbox = "日本語".into();
    let wire_archive = encode_modified_utf7(&account.archive_mailbox).expect("encode archive wire");
    store
        .save_account(&account)
        .await
        .expect("save fictional account");
    let service = MailService::new(store.clone());
    service
        .credentials()
        .set_password(&account, "sync secret")
        .await
        .expect("save fictional credential");

    let expected_wire = wire_archive.clone();
    let server = tokio::spawn(async move {
        let mut connection =
            accept_list_test_client(listener, "reader@example.test", "sync secret").await;
        assert_eq!(smtp_command(&mut connection).await, "D0003 CAPABILITY\r\n");
        smtp_reply(
            &mut connection,
            "* CAPABILITY IMAP4rev1 MOVE UIDPLUS\r\nD0003 OK capability\r\n",
        )
        .await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0004 LIST \"\" \"*\"\r\n"
        );
        smtp_reply(
            &mut connection,
            &format!(
                "* LIST (\\HasNoChildren \\XVendorFeature \\Archive) \"/\" {{{}}}\r\n",
                expected_wire.len()
            ),
        )
        .await;
        connection
            .get_mut()
            .write_all(expected_wire.as_bytes())
            .await
            .expect("write exact LIST literal");
        smtp_reply(&mut connection, "\r\nD0004 OK listed\r\n").await;
        assert_eq!(
            smtp_command(&mut connection).await,
            format!("D0005 SELECT {}\r\n", quote_imap(&expected_wire))
        );
        smtp_reply(
            &mut connection,
            "* 0 EXISTS\r\n* OK [UIDVALIDITY 88] valid\r\n* OK [UIDNEXT 1] next\r\nD0005 OK selected\r\n",
        )
        .await;
        assert_eq!(
            smtp_command(&mut connection).await,
            format!("D0006 SELECT {}\r\n", quote_imap(&expected_wire))
        );
        smtp_reply(
            &mut connection,
            "* 0 EXISTS\r\n* OK [UIDVALIDITY 88] valid\r\n* OK [UIDNEXT 1] next\r\nD0006 OK selected\r\n",
        )
        .await;
        assert_eq!(smtp_command(&mut connection).await, "D0007 LOGOUT\r\n");
        smtp_reply(&mut connection, "* BYE done\r\nD0007 OK logout\r\n").await;
    });

    let mut client =
        ImapClient::connect_with_endpoint(&account, imap_test_endpoint(account.imap_port, true))
            .await
            .expect("connect through production TLS client");
    let archive_plan = mailbox_plans(&account)
        .expect("encode configured mailbox plans")
        .into_iter()
        .filter(|plan| plan.local == "Archive")
        .collect::<Vec<_>>();
    service
        .sync_mailboxes_with_progress_on_client(
            &mut client,
            &account,
            50,
            archive_plan,
            false,
            |_| {},
        )
        .await
        .expect("sync exact literal archive name");
    server.await.expect("fictional IMAP server completed");
    drop(service);
    drop(store);

    let reopened = Store::open(&database)
        .await
        .expect("reopen fictional profile");
    assert_eq!(
        reopened
            .mailbox_catalog_state(account.id, "Archive")
            .await
            .expect("read reopened archive state")
            .expect("archive state was committed")
            .remote_name,
        wire_archive
    );
}

#[tokio::test]
async fn malformed_list_after_known_noselect_sent_mailbox_never_reaches_append() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fictional IMAP server");
    let account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
    let server = tokio::spawn(async move {
        let mut connection =
            accept_list_test_client(listener, "reader@example.test", "secret").await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0003 LIST \"\" \"*\"\r\n"
        );
        smtp_reply(
            &mut connection,
            "* LIST (\\NoSelect \\Sent) \"/\" \"Sent\"\r\n* LIST () \"/\" \"&AEE-\"\r\nD0003 OK listed\r\n",
        )
        .await;
        let mut command = String::new();
        let read = timeout(Duration::from_secs(1), connection.read_line(&mut command)).await;
        assert!(
            matches!(read, Ok(Ok(0)) | Ok(Err(_))),
            "a NoSelect Sent mailbox must not receive APPEND: {command:?}"
        );
    });

    let store = Store::in_memory().await.expect("open fictional profile");
    let service = MailService::new(store);
    let error = service
        .append_sent_copy_with_endpoint(
            &account,
            "secret",
            b"Subject: fictional\r\n\r\nbody\r\n",
            Some(imap_test_endpoint(account.imap_port, true)),
            Duration::from_secs(5),
        )
        .await
        .expect_err("a malformed LIST after a known NoSelect Sent mailbox must fail before APPEND");
    assert!(error.to_string().contains("modified UTF-7"));
    server.await.expect("fictional IMAP server completed");
}

#[tokio::test]
async fn known_noselect_source_never_reaches_select_or_move() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fictional IMAP server");
    let account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
    let server = tokio::spawn(async move {
        let mut connection =
            accept_list_test_client(listener, "reader@example.test", "secret").await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0003 LIST \"\" \"*\"\r\n"
        );
        smtp_reply(
            &mut connection,
            "* LIST (\\XVendorFeature \\NoSelect) \"/\" \"Projects\"\r\nD0003 OK listed\r\n",
        )
        .await;
        let mut command = String::new();
        let read = timeout(Duration::from_secs(1), connection.read_line(&mut command)).await;
        assert!(
            matches!(read, Ok(Ok(0)) | Ok(Err(_))),
            "a NoSelect source must not receive SELECT or UID MOVE: {command:?}"
        );
    });

    let mut client =
        ImapClient::connect_with_endpoint(&account, imap_test_endpoint(account.imap_port, true))
            .await
            .expect("connect through production TLS client");
    let error = MailService::apply_action_with_client_receipt(
        &mut client,
        &account,
        "secret",
        "Projects",
        42,
        MailboxAction::Archive,
        Some(77),
        None,
    )
    .await
    .expect_err("a known NoSelect source must fail before SELECT");
    assert!(error.to_string().contains("Noselect"));
    drop(client);
    server.await.expect("fictional IMAP server completed");
}

#[tokio::test]
async fn known_noselect_source_never_reaches_select_or_delete() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fictional IMAP server");
    let account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
    let server = tokio::spawn(async move {
        let mut connection =
            accept_list_test_client(listener, "reader@example.test", "secret").await;
        assert_eq!(smtp_command(&mut connection).await, "D0003 CAPABILITY\r\n");
        smtp_reply(
            &mut connection,
            "* CAPABILITY IMAP4rev1 UIDPLUS\r\nD0003 OK capability\r\n",
        )
        .await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0004 LIST \"\" \"*\"\r\n"
        );
        smtp_reply(
            &mut connection,
            "* LIST (\\NoSelect) \"/\" \"Projects\"\r\nD0004 OK listed\r\n",
        )
        .await;
        let mut command = String::new();
        let read = timeout(Duration::from_secs(1), connection.read_line(&mut command)).await;
        assert!(
            matches!(read, Ok(Ok(0)) | Ok(Err(_))),
            "a NoSelect source must not receive SELECT, STORE, or EXPUNGE: {command:?}"
        );
    });

    let mut client =
        ImapClient::connect_with_endpoint(&account, imap_test_endpoint(account.imap_port, true))
            .await
            .expect("connect through production TLS client");
    let error = MailService::apply_action_with_client_receipt(
        &mut client,
        &account,
        "secret",
        "Projects",
        42,
        MailboxAction::Delete,
        Some(77),
        None,
    )
    .await
    .expect_err("a known NoSelect source must fail before permanent delete selection");
    assert!(error.to_string().contains("Noselect"));
    drop(client);
    server.await.expect("fictional IMAP server completed");
}
