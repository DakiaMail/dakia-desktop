//! Production TLS coverage for the logical-name to IMAP wire-name boundary.

use super::*;
use crate::{imap_mailbox::encode_modified_utf7, storage::CachedMessageContent};
use tokio::{
    io::AsyncReadExt,
    time::{timeout, Duration},
};

async fn accept_imap_client(
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

fn mailbox_message(account: &Account, mailbox: &str, uid: u32) -> MailSummary {
    let headers = format!(
        "Date: Wed, 30 Jul 2026 10:00:00 +0000\r\nFrom: Sender <sender@example.test>\r\nTo: Reader <reader@example.test>\r\nSubject: Unicode mailbox wire fixture\r\nMessage-ID: <wire-{uid}@example.test>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n"
    );
    let mut message = parse_catalog_message(
        account,
        mailbox,
        uid,
        &[format!(
            "* 1 FETCH (UID {uid} FLAGS () INTERNALDATE \"30-Jul-2026 10:00:00 +0000\")\r\n"
        )],
        headers.as_bytes(),
        "preserved preview".into(),
    )
    .expect("parse fictional mailbox message");
    message.mailbox_uid_validity = Some(77);
    message
}

#[test]
fn configured_mailbox_dual_format_preserves_legacy_wire_and_encodes_logical_names_once() {
    let mut account = test_account();
    account.archive_mailbox = "日本語".into();
    assert_eq!(
        configured_remote_mailbox_wire(&account, "Archive").unwrap(),
        "&ZeVnLIqe-"
    );

    account.archive_mailbox = "&ZeVnLIqe-".into();
    assert_eq!(
        configured_remote_mailbox_wire(&account, "Archive").unwrap(),
        "&ZeVnLIqe-",
        "previous-stable canonical wire configuration must not be encoded twice"
    );

    account.archive_mailbox = "R&D".into();
    assert_eq!(
        configured_remote_mailbox_wire(&account, "Archive").unwrap(),
        "R&-D"
    );

    account.archive_mailbox = "&-ZeVnLIqe-".into();
    assert_eq!(
        configured_remote_mailbox_wire(&account, "Archive").unwrap(),
        "&-ZeVnLIqe-",
        "canonical-looking ASCII is intentionally treated as legacy wire"
    );
}

#[tokio::test]
async fn generic_unicode_archive_sync_selects_modified_utf7_and_reopens_exact_wire_name() {
    let profile = tempfile::tempdir().expect("temporary fictional profile");
    let database = profile.path().join("unicode-archive-sync.sqlite");
    let store = Store::open(&database)
        .await
        .expect("open fictional profile");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fictional IMAP server");
    let mut account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
    let logical_archive = "归档/雪";
    let wire_archive = encode_modified_utf7(logical_archive).expect("encode archive wire name");
    account.archive_mailbox = logical_archive.into();
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
            accept_imap_client(listener, "reader@example.test", "sync secret").await;
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
                "* LIST (\\HasNoChildren \\Archive) \"/\" {}\r\nD0004 OK listed\r\n",
                quote_imap(&expected_wire)
            ),
        )
        .await;
        let select = format!("D0005 SELECT {}\r\n", quote_imap(&expected_wire));
        assert_eq!(smtp_command(&mut connection).await, select);
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
        .expect("sync configured Unicode archive");
    server.await.expect("fictional IMAP server completed");
    drop(service);
    drop(store);

    let reopened = Store::open(&database)
        .await
        .expect("reopen fictional profile");
    let state = reopened
        .mailbox_catalog_state(account.id, "Archive")
        .await
        .expect("read reopened archive state")
        .expect("archive state was committed");
    assert_eq!(state.remote_name, wire_archive);
    assert_eq!(state.uid_validity, 88);
}

#[tokio::test]
async fn discovered_unicode_special_use_destination_overrides_missing_configured_folder_for_move() {
    let profile = tempfile::tempdir().expect("temporary fictional profile");
    let database = profile.path().join("unicode-move.sqlite");
    let store = Store::open(&database)
        .await
        .expect("open fictional profile");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fictional IMAP server");
    let account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
    let logical_source = "项目/收件";
    let logical_destination = "日本語";
    let wire_source = encode_modified_utf7(logical_source).expect("encode source wire name");
    let wire_destination =
        encode_modified_utf7(logical_destination).expect("encode destination wire name");
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
    store
        .save_mailbox_catalog_state(account.id, "Projects", &wire_source, 77, 1, true)
        .await
        .expect("save exact source wire identity");
    store
        .save_mailbox_catalog_state(account.id, "Archive", &wire_destination, 88, 0, true)
        .await
        .expect("save exact destination wire identity");
    let message = mailbox_message(&account, "Projects", 42);
    let message_id = message.id.clone();
    store
        .upsert_catalog_messages(std::slice::from_ref(&message))
        .await
        .expect("save fictional source message");

    let expected_source = wire_source.clone();
    let expected_destination = wire_destination.clone();
    let server = tokio::spawn(async move {
        let mut connection =
            accept_imap_client(listener, "reader@example.test", "sync secret").await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0003 LIST \"\" \"*\"\r\n"
        );
        smtp_reply(
            &mut connection,
            &format!(
                "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n* LIST (\\HasNoChildren) \"/\" {}\r\n* LIST (\\HasNoChildren \\Archive) \"/\" {}\r\nD0003 OK listed\r\n",
                quote_imap(&expected_source),
                quote_imap(&expected_destination)
            ),
        )
        .await;
        assert_eq!(
            smtp_command(&mut connection).await,
            format!("D0004 SELECT {}\r\n", quote_imap(&expected_source))
        );
        smtp_reply(
            &mut connection,
            "* 1 EXISTS\r\n* OK [UIDVALIDITY 77] valid\r\nD0004 OK selected\r\n",
        )
        .await;
        assert_eq!(
            smtp_command(&mut connection).await,
            format!(
                "D0005 UID MOVE 42 {}\r\n",
                quote_imap(&expected_destination)
            )
        );
        smtp_reply(&mut connection, "D0005 OK [COPYUID 88 42 207] moved\r\n").await;
        assert_eq!(smtp_command(&mut connection).await, "D0006 LOGOUT\r\n");
        smtp_reply(&mut connection, "* BYE done\r\nD0006 OK logout\r\n").await;
    });

    let receipt = service
        .apply_action_at_epoch_with_endpoint_for_test(
            &account,
            "Projects",
            42,
            MailboxAction::Archive,
            77,
            imap_test_endpoint(account.imap_port, true),
        )
        .await
        .expect("move through production action path");
    server.await.expect("fictional IMAP server completed");
    assert_eq!(
        receipt.destination,
        Some(MailboxMoveDestination {
            remote_mailbox: wire_destination.clone(),
            uid_validity: 88,
            uid: 207,
        })
    );
    let target = store
        .message(&message_id)
        .await
        .expect("read source message")
        .expect("source message exists");
    let publication = store
        .move_message_at_epoch(&target, "Archive", &receipt)
        .await
        .expect("publish exact move receipt");
    assert!(publication.source_published);
    assert!(publication.destination_mapped);
    drop(store);

    let reopened = Store::open(&database)
        .await
        .expect("reopen fictional profile");
    assert!(reopened
        .message_by_locator(account.id, "Projects", 42)
        .await
        .expect("read reopened source")
        .is_none());
    assert_eq!(
        reopened
            .message_by_locator(account.id, "Archive", 207)
            .await
            .expect("read reopened destination")
            .expect("destination mapping survives restart")
            .subject,
        "Unicode mailbox wire fixture"
    );
    assert_eq!(
        reopened
            .mailbox_catalog_state(account.id, "Archive")
            .await
            .expect("read reopened destination state")
            .expect("destination state survives restart")
            .remote_name,
        wire_destination
    );
}

#[tokio::test]
async fn discovered_unicode_sent_wire_name_is_appended_byte_exact_without_double_encoding() {
    let smtp_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fictional SMTP server");
    let imap_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fictional IMAP server");
    let mut account = smtp_test_account(Security::Tls, smtp_listener.local_addr().unwrap().port());
    account.imap_host = "127.0.0.1".into();
    account.imap_port = imap_listener.local_addr().unwrap().port();
    account.imap_security = Security::Tls;
    let store = Store::in_memory().await.expect("open fictional store");
    store
        .save_account(&account)
        .await
        .expect("save fictional account");
    let service = MailService::new(store);
    service
        .credentials()
        .set_password(&account, "secret")
        .await
        .expect("save fictional credential");
    let draft = ComposeMessage {
        account_id: account.id,
        to: vec!["recipient@example.test".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "Unicode Sent wire fixture".into(),
        body_text: "A fictional generic-provider message.".into(),
        body_html: None,
        in_reply_to: None,
        references: None,
        attachments: Vec::new(),
    };
    let logical_sent = "已发送/雪";
    let wire_sent = encode_modified_utf7(logical_sent).expect("encode Sent wire name");

    let smtp_server = tokio::spawn(async move {
        let (stream, _) = smtp_listener
            .accept()
            .await
            .expect("accept fictional SMTP client");
        let stream = smtp_test_acceptor()
            .accept(stream)
            .await
            .expect("accept fictional SMTP TLS");
        let mut connection = BufReader::new(stream);
        smtp_reply(&mut connection, "220 fictional SMTP ready\r\n").await;
        smtp_expect_ehlo_and_auth(&mut connection, false).await;
        smtp_expect_envelope_until_data(&mut connection).await;
        let message = smtp_read_message(&mut connection).await;
        assert!(message.contains("Subject: Unicode Sent wire fixture"));
        smtp_reply(&mut connection, "250 2.0.0 accepted once\r\n").await;
    });

    let expected_wire = wire_sent.clone();
    let imap_server = tokio::spawn(async move {
        let mut connection =
            accept_imap_client(imap_listener, "sender@example.test", "secret").await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0003 LIST \"\" \"*\"\r\n"
        );
        smtp_reply(
            &mut connection,
            &format!(
                "* LIST (\\HasNoChildren \\Sent) \"/\" {}\r\nD0003 OK listed\r\n",
                quote_imap(&expected_wire)
            ),
        )
        .await;
        let append = smtp_command(&mut connection).await;
        let prefix = format!("D0004 APPEND {} (\\Seen) {{", quote_imap(&expected_wire));
        let suffix = "}\r\n";
        assert!(
            append.starts_with(&prefix),
            "unexpected APPEND command: {append:?}"
        );
        assert!(
            append.ends_with(suffix),
            "unexpected APPEND command: {append:?}"
        );
        let declared_length = append[prefix.len()..append.len() - suffix.len()]
            .parse::<usize>()
            .expect("APPEND literal length is decimal");
        assert_eq!(
            append,
            format!("{prefix}{declared_length}{suffix}"),
            "the full APPEND transcript must contain the exact discovered wire name"
        );
        smtp_reply(&mut connection, "+ send literal\r\n").await;
        let mut literal = vec![0_u8; declared_length + 2];
        connection
            .read_exact(&mut literal)
            .await
            .expect("read exact APPEND literal");
        assert_eq!(&literal[declared_length..], b"\r\n");
        assert!(
            String::from_utf8_lossy(&literal[..declared_length])
                .contains("Subject: Unicode Sent wire fixture"),
            "APPEND literal must be the submitted fictional message"
        );
        smtp_reply(&mut connection, "D0004 OK [APPENDUID 91 501] appended\r\n").await;
        assert_eq!(smtp_command(&mut connection).await, "D0005 LOGOUT\r\n");
        smtp_reply(&mut connection, "* BYE done\r\nD0005 OK logout\r\n").await;
    });

    service
        .send_with_endpoints_for_test(
            &account,
            &draft,
            smtp_test_endpoint(&account),
            Some(imap_test_endpoint(account.imap_port, true)),
        )
        .await
        .expect("submit once and save exact Sent copy");
    smtp_server.await.expect("fictional SMTP server completed");
    imap_server.await.expect("fictional IMAP server completed");
}

#[tokio::test]
async fn noncanonical_modified_utf7_list_is_refused_before_select_and_preserves_reopened_state() {
    let profile = tempfile::tempdir().expect("temporary fictional profile");
    let database = profile.path().join("noncanonical-list.sqlite");
    let store = Store::open(&database)
        .await
        .expect("open fictional profile");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fictional IMAP server");
    let mut account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
    account.archive_mailbox = "存档".into();
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
    let canonical_wire = encode_modified_utf7(&account.archive_mailbox)
        .expect("encode configured archive wire name");
    store
        .save_mailbox_catalog_state(account.id, "Archive", &canonical_wire, 77, 1, true)
        .await
        .expect("save baseline archive state");
    let mut committed = mailbox_message(&account, "Archive", 42);
    committed.body_text = "durable catalogue body".into();
    committed.content_state = "complete".into();
    let committed_id = committed.id.clone();
    store
        .upsert_messages(std::slice::from_ref(&committed))
        .await
        .expect("save baseline message");
    store
        .cache_message_content(
            &committed_id,
            false,
            CachedMessageContent {
                body_text: "durable cached body".into(),
                body_html: Some("<p>durable cached body</p>".into()),
                unsubscribe_kind: None,
                attachments: Vec::new(),
            },
        )
        .await
        .expect("save baseline cache");
    let baseline_message = serde_json::to_value(
        store
            .message(&committed_id)
            .await
            .expect("read baseline message")
            .expect("baseline message exists"),
    )
    .expect("serialize baseline message");
    let baseline_cached = store
        .cached_message_content(&committed_id)
        .await
        .expect("read baseline cache")
        .expect("baseline cache exists before malformed LIST");
    assert_eq!(baseline_cached.body_text, "durable cached body");

    let server = tokio::spawn(async move {
        let mut connection =
            accept_imap_client(listener, "reader@example.test", "sync secret").await;
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
        // &AEE- decodes to printable ASCII `A`, which modified UTF-7 must
        // represent directly. Accepting it would give one logical mailbox two
        // distinct wire identities.
        smtp_reply(
            &mut connection,
            "* LIST (\\HasNoChildren \\Archive) \"/\" \"&AEE-\"\r\nD0004 OK listed\r\n",
        )
        .await;
        let mut forbidden = String::new();
        match timeout(Duration::from_secs(1), connection.read_line(&mut forbidden)).await {
            Ok(Ok(0)) | Ok(Err(_)) => {}
            Ok(Ok(_)) => panic!("noncanonical LIST triggered a command: {forbidden:?}"),
            Err(_) => panic!("client kept the rejected LIST connection open"),
        }
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
    let error = service
        .sync_mailboxes_with_progress_on_client(
            &mut client,
            &account,
            50,
            archive_plan,
            true,
            |_| {},
        )
        .await
        .expect_err("noncanonical modified UTF-7 LIST name must be rejected");
    let rejection = format!("{error:#}");
    assert!(
        rejection.contains("modified UTF-7") || rejection.contains("mailbox name"),
        "rejection should identify the unsafe mailbox name: {rejection}"
    );
    drop(client);
    server.await.expect("fictional IMAP refusal completed");
    drop(service);
    drop(store);

    let reopened = Store::open(&database)
        .await
        .expect("reopen fictional profile");
    let state = reopened
        .mailbox_catalog_state(account.id, "Archive")
        .await
        .expect("read reopened archive state")
        .expect("baseline archive state remains");
    assert_eq!(state.remote_name, canonical_wire);
    assert_eq!(state.uid_validity, 77);
    assert_eq!(state.remote_total, 1);
    let message = serde_json::to_value(
        reopened
            .message(&committed_id)
            .await
            .expect("read reopened message")
            .expect("baseline message remains"),
    )
    .expect("serialize reopened message");
    assert_eq!(message, baseline_message);
    let cached = reopened
        .cached_message_content(&committed_id)
        .await
        .expect("read reopened cache")
        .expect("baseline cache remains");
    assert_eq!(cached.body_text, baseline_cached.body_text);
    assert_eq!(cached.body_html, baseline_cached.body_html);
    assert_eq!(cached.unsubscribe_kind, baseline_cached.unsubscribe_kind);
    assert_eq!(cached.attachments.len(), baseline_cached.attachments.len());
}
