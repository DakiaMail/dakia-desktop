//! Production-boundary regressions for authoritative IMAP search.

use super::*;
use crate::{storage::CachedMessageContent, SearchQuery};
use std::collections::{HashMap, HashSet};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt},
    sync::oneshot,
    time::{timeout, Duration},
};

fn search_message(account: &Account, mailbox: &str, uid: u32, subject: &str) -> MailSummary {
    let headers = format!(
        "Date: Wed, 30 Jul 2026 10:00:00 +0000\r\nFrom: Sender <sender@example.test>\r\nTo: Reader <reader@example.test>\r\nSubject: {subject}\r\nMessage-ID: <{mailbox}-{uid}-{}@example.test>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n",
        account.id
    );
    parse_catalog_message(
        account,
        mailbox,
        uid,
        &[format!(
            "* 1 FETCH (UID {uid} FLAGS () INTERNALDATE \"30-Jul-2026 10:00:00 +0000\")\r\n"
        )],
        headers.as_bytes(),
        String::new(),
    )
    .unwrap()
}

async fn seed_search_account(
    store: &Store,
    service: &MailService,
    account: &Account,
    mailboxes: &[(&str, &str, u32)],
) {
    store.save_account(account).await.unwrap();
    service
        .credentials()
        .set_password(account, "search secret")
        .await
        .unwrap();
    for (mailbox, remote, uid_validity) in mailboxes {
        store
            .save_mailbox_catalog_state(account.id, mailbox, remote, *uid_validity, 0, true)
            .await
            .unwrap();
    }
}

async fn accept_search_client(
    listener: TcpListener,
) -> BufReader<tokio_rustls::server::TlsStream<TcpStream>> {
    let (stream, _) = listener.accept().await.unwrap();
    let stream = smtp_test_acceptor().accept(stream).await.unwrap();
    let mut connection = BufReader::new(stream);
    smtp_reply(&mut connection, "* OK fictional IMAP ready\r\n").await;
    assert_eq!(smtp_command(&mut connection).await, "D0001 CAPABILITY\r\n");
    smtp_reply(
        &mut connection,
        "* CAPABILITY IMAP4rev1\r\nD0001 OK capability\r\n",
    )
    .await;
    assert_eq!(
        smtp_command(&mut connection).await,
        "D0002 LOGIN \"reader@example.test\" \"search secret\"\r\n"
    );
    smtp_reply(&mut connection, "D0002 OK authenticated\r\n").await;
    connection
}

#[tokio::test]
async fn production_imap_connector_rejects_utf8_only_before_login_and_keeps_utf8_accept_compatible()
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = smtp_test_acceptor().accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        smtp_reply(&mut connection, "* OK fictional IMAP ready\r\n").await;
        assert_eq!(smtp_command(&mut connection).await, "D0001 CAPABILITY\r\n");
        smtp_reply(
            &mut connection,
            "* CAPABILITY IMAP4rev1 UTF8=ONLY\r\nD0001 OK capability\r\n",
        )
        .await;
        assert_connection_closes_without_command(&mut connection, "UTF8=ONLY capability").await;
    });

    let error = match ImapClient::connect_with_endpoint(
        &account,
        imap_test_endpoint(account.imap_port, true),
    )
    .await
    {
        Ok(_) => panic!("UTF8=ONLY server unexpectedly connected"),
        Err(error) => error,
    };
    assert_eq!(error.to_string(), "IMAP UTF8=ONLY mode is not supported");
    server.await.unwrap();

    validate_imap_auth_capabilities(&account, &["* CAPABILITY IMAP4rev1 UTF8=ACCEPT".into()])
        .expect("UTF8=ACCEPT must remain compatible with the legacy IMAP4rev1 path");
}

#[tokio::test]
async fn production_imap_connector_rejects_rev2_only_before_login_but_allows_dual_profile_servers()
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = smtp_test_acceptor().accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        smtp_reply(&mut connection, "* OK fictional IMAP ready\r\n").await;
        assert_eq!(smtp_command(&mut connection).await, "D0001 CAPABILITY\r\n");
        smtp_reply(
            &mut connection,
            "* CAPABILITY IMAP4rev2\r\nD0001 OK capability\r\n",
        )
        .await;
        assert_connection_closes_without_command(&mut connection, "IMAP4rev2-only capability")
            .await;
    });

    let error = match ImapClient::connect_with_endpoint(
        &account,
        imap_test_endpoint(account.imap_port, true),
    )
    .await
    {
        Ok(_) => panic!("IMAP4rev2-only server unexpectedly connected"),
        Err(error) => error,
    };
    assert_eq!(
        error.to_string(),
        "IMAP server did not advertise a supported IMAP profile after TLS"
    );
    server.await.unwrap();

    validate_imap_auth_capabilities(&account, &["* CAPABILITY IMAP4rev1 IMAP4rev2".into()])
        .expect("dual-profile servers remain compatible through their IMAP4rev1 capability");
}

async fn assert_listener_unused(listener: TcpListener, context: &str) {
    match timeout(Duration::from_millis(300), listener.accept()).await {
        Err(_) => {}
        Ok(Ok(_)) => panic!("{context} connected before rejecting unsafe IMAP input"),
        Ok(Err(error)) => panic!("{context} listener failed unexpectedly: {error}"),
    }
}

async fn assert_connection_closes_without_command(
    connection: &mut BufReader<tokio_rustls::server::TlsStream<TcpStream>>,
    context: &str,
) {
    let mut forbidden = String::new();
    match timeout(Duration::from_secs(1), connection.read_line(&mut forbidden)).await {
        Ok(Ok(0)) | Ok(Err(_)) => {}
        Ok(Ok(_)) => panic!("{context} wrote a command after rejection: {forbidden:?}"),
        Err(_) => panic!("{context} left the rejected provider connection open"),
    }
}

async fn accept_utf8_search_literal(
    connection: &mut BufReader<tokio_rustls::server::TlsStream<TcpStream>>,
    tag: &str,
    text: &str,
) {
    assert_eq!(
        smtp_command(connection).await,
        format!("{tag} UID SEARCH CHARSET UTF-8 TEXT {{{}}}\r\n", text.len())
    );
    let mut premature = [0u8; 1];
    assert!(
        timeout(
            Duration::from_millis(150),
            connection.read_exact(&mut premature)
        )
        .await
        .is_err(),
        "the client sent UTF-8 literal bytes before the server continuation"
    );
    smtp_reply(connection, "+ send the UTF-8 search key\r\n").await;
    let mut received = vec![0u8; text.len() + 2];
    connection.read_exact(&mut received).await.unwrap();
    assert_eq!(&received[..text.len()], text.as_bytes());
    assert_eq!(&received[text.len()..], b"\r\n");
}

async fn assert_remote_search_boundary(requested_limit: usize) {
    let store = Store::in_memory().await.unwrap();
    let service = MailService::new(store.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
    seed_search_account(
        &store,
        &service,
        &account,
        &[("INBOX", "INBOX", 77), ("Archive", "Archive", 88)],
    )
    .await;

    let messages = (1..=501)
        .map(|uid| search_message(&account, "INBOX", uid, &format!("bounded hit {uid}")))
        .collect::<Vec<_>>();
    store.upsert_catalog_messages(&messages).await.unwrap();
    store
        .upsert_catalog_messages(&[search_message(&account, "Archive", 501, "wrong mailbox")])
        .await
        .unwrap();

    let mut other_account = account.clone();
    other_account.id = uuid::Uuid::new_v4();
    other_account.email = "other@example.test".into();
    other_account.auth = AccountAuth::Password {
        username: "other@example.test".into(),
    };
    store.save_account(&other_account).await.unwrap();
    store
        .save_mailbox_catalog_state(other_account.id, "INBOX", "INBOX", 77, 501, true)
        .await
        .unwrap();
    store
        .upsert_catalog_messages(&[search_message(
            &other_account,
            "INBOX",
            501,
            "wrong account",
        )])
        .await
        .unwrap();

    let server = tokio::spawn(async move {
        let mut connection = accept_search_client(listener).await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0003 LIST \"\" \"*\"\r\n"
        );
        smtp_reply(
            &mut connection,
            "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n* LIST (\\HasNoChildren) \"/\" \"Archive\"\r\nD0003 OK listed\r\n",
        )
        .await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0004 SELECT \"INBOX\"\r\n"
        );
        smtp_reply(
            &mut connection,
            "* 501 EXISTS\r\n* OK [UIDVALIDITY 77] valid\r\nD0004 OK selected\r\n",
        )
        .await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0005 UID SEARCH TEXT \"boundary\"\r\n"
        );
        let provider_uids = (1..=501)
            .map(|uid| uid.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        smtp_reply(
            &mut connection,
            &format!("* SEARCH {provider_uids}\r\nD0005 OK searched\r\n"),
        )
        .await;
        assert_eq!(smtp_command(&mut connection).await, "D0006 LOGOUT\r\n");
        smtp_reply(&mut connection, "* BYE done\r\nD0006 OK logout\r\n").await;
    });

    let results = service
        .search_remote_with_imap_endpoint_for_test(
            &account,
            "boundary",
            Some("INBOX"),
            requested_limit,
            imap_test_endpoint(account.imap_port, true),
        )
        .await
        .unwrap();
    server.await.unwrap();

    let expected_uids = (1..=501).rev().take(requested_limit).collect::<Vec<_>>();
    assert_eq!(
        results
            .iter()
            .map(|message| message.uid)
            .collect::<Vec<_>>(),
        expected_uids,
        "the actual provider response must stop exactly at the requested boundary"
    );
    assert_eq!(
        results
            .iter()
            .map(|message| (&message.account_id, &message.mailbox, message.uid))
            .collect::<std::collections::HashSet<_>>()
            .len(),
        requested_limit,
        "every returned locator must remain unique"
    );
    assert!(results.iter().all(|message| {
        message.account_id == account.id.to_string()
            && message.mailbox == "INBOX"
            && message.subject != "wrong account"
            && message.subject != "wrong mailbox"
    }));
}

#[tokio::test]
async fn remote_search_provider_results_are_exactly_bounded_at_499_500_and_501() {
    for requested_limit in [499, 500, 501] {
        assert_remote_search_boundary(requested_limit).await;
    }
}

#[tokio::test]
async fn remote_search_rejects_query_command_controls_without_writing_or_losing_state() {
    for unsafe_query in [
        "safe\r\nD9999 NOOP\r\ntrailer",
        "safe\nD9999 NOOP\ntrailer",
        "safe\0tail",
        "\r\nsafe",
        "safe\r\n",
        "\nsafe",
        "safe\n",
        "\0safe",
        "safe\0",
    ] {
        let store = Store::in_memory().await.unwrap();
        let service = MailService::new(store.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
        seed_search_account(&store, &service, &account, &[("INBOX", "INBOX", 77)]).await;
        let committed = search_message(&account, "INBOX", 42, "must survive rejection");
        let committed_id = committed.id.clone();
        store.upsert_catalog_messages(&[committed]).await.unwrap();

        let server = tokio::spawn(assert_listener_unused(listener, "unsafe search query"));

        let error = service
            .search_remote_with_imap_endpoint_for_test(
                &account,
                unsafe_query,
                Some("INBOX"),
                10,
                imap_test_endpoint(account.imap_port, true),
            )
            .await
            .unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("prohibited framing bytes"), "{rendered}");
        assert!(!rendered.contains(unsafe_query));
        server.await.unwrap();
        let preserved = store.message(&committed_id).await.unwrap().unwrap();
        assert_eq!(preserved.uid, 42);
        assert_eq!(preserved.account_id, account.id.to_string());
        assert_eq!(preserved.mailbox, "INBOX");
    }
}

#[tokio::test]
async fn remote_search_rejects_configured_mailbox_controls_without_writing_or_losing_state() {
    for unsafe_mailbox in [
        "Archive\r\nD9999 NOOP\r\ntrailer",
        "Archive\nD9999 NOOP\ntrailer",
        "Archive\0tail",
    ] {
        let store = Store::in_memory().await.unwrap();
        let service = MailService::new(store.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
        account.archive_mailbox = unsafe_mailbox.into();
        seed_search_account(
            &store,
            &service,
            &account,
            &[("Archive", unsafe_mailbox, 88)],
        )
        .await;
        let committed = search_message(&account, "Archive", 43, "must survive rejection");
        let committed_id = committed.id.clone();
        store.upsert_catalog_messages(&[committed]).await.unwrap();

        let server = tokio::spawn(assert_listener_unused(listener, "unsafe mailbox"));

        let error = service
            .search_remote_with_imap_endpoint_for_test(
                &account,
                "boundary",
                Some("Archive"),
                10,
                imap_test_endpoint(account.imap_port, true),
            )
            .await
            .unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("prohibited framing bytes"), "{rendered}");
        assert!(!rendered.contains(unsafe_mailbox));
        server.await.unwrap();
        let preserved = store.message(&committed_id).await.unwrap().unwrap();
        assert_eq!(preserved.uid, 43);
        assert_eq!(preserved.account_id, account.id.to_string());
        assert_eq!(preserved.mailbox, "Archive");
    }
}

#[tokio::test]
async fn remote_search_rejects_requested_mailbox_controls_without_connecting_or_losing_state() {
    for unsafe_mailbox in [
        "INBOX\r\nD9999 NOOP\r\ntrailer",
        "INBOX\nD9999 NOOP\ntrailer",
        "INBOX\0tail",
    ] {
        let store = Store::in_memory().await.unwrap();
        let service = MailService::new(store.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
        seed_search_account(&store, &service, &account, &[("INBOX", "INBOX", 77)]).await;
        let committed = search_message(&account, "INBOX", 45, "must survive rejection");
        let committed_id = committed.id.clone();
        store.upsert_catalog_messages(&[committed]).await.unwrap();

        let server = tokio::spawn(assert_listener_unused(listener, "unsafe requested mailbox"));
        let error = service
            .search_remote_with_imap_endpoint_for_test(
                &account,
                "boundary",
                Some(unsafe_mailbox),
                10,
                imap_test_endpoint(account.imap_port, true),
            )
            .await
            .unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("prohibited framing bytes"), "{rendered}");
        assert!(!rendered.contains(unsafe_mailbox));
        server.await.unwrap();
        let preserved = store.message(&committed_id).await.unwrap().unwrap();
        assert_eq!(preserved.uid, 45);
        assert_eq!(preserved.account_id, account.id.to_string());
        assert_eq!(preserved.mailbox, "INBOX");
    }
}

async fn assert_remote_search_select_refusal(mailbox: Option<&str>, selected_mailbox: &str) {
    let store = Store::in_memory().await.unwrap();
    let service = MailService::new(store.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
    seed_search_account(
        &store,
        &service,
        &account,
        &[("INBOX", "INBOX", 77), ("Archive", "Archive", 88)],
    )
    .await;
    let committed = search_message(&account, "INBOX", 44, "must survive select refusal");
    let committed_id = committed.id.clone();
    store.upsert_catalog_messages(&[committed]).await.unwrap();

    let expected_select = format!("D0004 SELECT {selected_mailbox:?}\r\n");
    let server = tokio::spawn(async move {
        let mut connection = accept_search_client(listener).await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0003 LIST \"\" \"*\"\r\n"
        );
        smtp_reply(&mut connection, "D0003 OK listed\r\n").await;
        assert_eq!(smtp_command(&mut connection).await, expected_select);
        smtp_reply(
            &mut connection,
            "D0004 NO [ALERT] private-provider-diagnostic\r\n",
        )
        .await;
        assert_connection_closes_without_command(&mut connection, "SELECT refusal").await;
    });

    let error = service
        .search_remote_with_imap_endpoint_for_test(
            &account,
            "boundary",
            mailbox,
            10,
            imap_test_endpoint(account.imap_port, true),
        )
        .await
        .unwrap_err();
    let rendered = error.to_string();
    assert!(
        rendered.to_ascii_lowercase().contains("select"),
        "{rendered}"
    );
    assert!(!rendered.contains("private-provider-diagnostic"));
    server.await.unwrap();
    let preserved = store.message(&committed_id).await.unwrap().unwrap();
    assert_eq!(preserved.uid, 44);
    assert_eq!(preserved.account_id, account.id.to_string());
    assert_eq!(preserved.mailbox, "INBOX");
}

#[tokio::test]
async fn remote_search_reports_scoped_and_unified_select_refusal_without_searching_or_losing_state()
{
    assert_remote_search_select_refusal(Some("Archive"), "Archive").await;
    assert_remote_search_select_refusal(None, "INBOX").await;
}

#[tokio::test]
async fn remote_search_escapes_unicode_and_respects_account_mailbox_and_limit() {
    let store = Store::in_memory().await.unwrap();
    let service = MailService::new(store.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
    seed_search_account(
        &store,
        &service,
        &account,
        &[("INBOX", "INBOX", 77), ("Archive", "Archive", 88)],
    )
    .await;
    store
        .upsert_catalog_messages(&[
            search_message(&account, "INBOX", 1, "oldest allowed"),
            search_message(&account, "INBOX", 2, "second allowed"),
            search_message(&account, "INBOX", 3, "newest allowed"),
            search_message(&account, "Archive", 3, "wrong mailbox"),
        ])
        .await
        .unwrap();

    let mut other_account = account.clone();
    other_account.id = uuid::Uuid::new_v4();
    other_account.email = "other@example.test".into();
    other_account.auth = AccountAuth::Password {
        username: "other@example.test".into(),
    };
    store.save_account(&other_account).await.unwrap();
    store
        .save_mailbox_catalog_state(other_account.id, "INBOX", "INBOX", 77, 1, true)
        .await
        .unwrap();
    store
        .upsert_catalog_messages(&[search_message(&other_account, "INBOX", 3, "wrong account")])
        .await
        .unwrap();

    let server = tokio::spawn(async move {
        let mut connection = accept_search_client(listener).await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0003 LIST \"\" \"*\"\r\n"
        );
        smtp_reply(
            &mut connection,
            "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\nD0003 OK listed\r\n",
        )
        .await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0004 SELECT \"INBOX\"\r\n"
        );
        smtp_reply(
            &mut connection,
            "* 3 EXISTS\r\n* OK [UIDVALIDITY 77] valid\r\nD0004 OK selected\r\n",
        )
        .await;
        accept_utf8_search_literal(&mut connection, "D0005", "雪 \"töö\" \\ path").await;
        smtp_reply(&mut connection, "* SEARCH 1 3 2\r\nD0005 OK searched\r\n").await;
        assert_eq!(smtp_command(&mut connection).await, "D0006 LOGOUT\r\n");
        smtp_reply(&mut connection, "* BYE done\r\nD0006 OK logout\r\n").await;
    });

    let results = service
        .search_remote_with_imap_endpoint_for_test(
            &account,
            "  雪 \"töö\" \\ path  ",
            Some("INBOX"),
            2,
            imap_test_endpoint(account.imap_port, true),
        )
        .await
        .unwrap();
    server.await.unwrap();

    assert_eq!(
        results
            .iter()
            .map(|message| message.uid)
            .collect::<Vec<_>>(),
        vec![3, 2],
        "the provider result is newest-first and bounded by the requested limit"
    );
    assert!(results.iter().all(|message| {
        message.account_id == account.id.to_string() && message.mailbox == "INBOX"
    }));
    assert!(results
        .iter()
        .all(|message| message.subject != "wrong account" && message.subject != "wrong mailbox"));
}

#[tokio::test]
async fn gmail_remote_search_falls_back_from_rejected_raw_query_without_widening_scope() {
    let store = Store::in_memory().await.unwrap();
    let service = MailService::new(store.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
    account.provider_id = "gmail".into();
    account.archive_mailbox = "[Gmail]/All Mail".into();
    account.spam_mailbox = "[Gmail]/Spam".into();
    seed_search_account(&store, &service, &account, &[("INBOX", "INBOX", 77)]).await;
    store
        .upsert_catalog_messages(&[search_message(&account, "INBOX", 42, "fallback hit")])
        .await
        .unwrap();

    let server = tokio::spawn(async move {
        let mut connection = accept_search_client(listener).await;
        assert_eq!(smtp_command(&mut connection).await, "D0003 CAPABILITY\r\n");
        smtp_reply(
            &mut connection,
            "* CAPABILITY IMAP4rev1 X-GM-EXT-1\r\nD0003 OK capability\r\n",
        )
        .await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0004 LIST \"\" \"*\"\r\n"
        );
        smtp_reply(
            &mut connection,
            "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\nD0004 OK listed\r\n",
        )
        .await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0005 SELECT \"INBOX\"\r\n"
        );
        smtp_reply(
            &mut connection,
            "* 1 EXISTS\r\n* OK [UIDVALIDITY 77] valid\r\nD0005 OK selected\r\n",
        )
        .await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0006 UID SEARCH X-GM-RAW \"from:例@example.test\"\r\n"
        );
        smtp_reply(&mut connection, "D0006 BAD raw search unavailable\r\n").await;
        accept_utf8_search_literal(&mut connection, "D0007", "from:例@example.test").await;
        smtp_reply(&mut connection, "* SEARCH 42\r\nD0007 OK searched\r\n").await;
        assert_eq!(smtp_command(&mut connection).await, "D0008 LOGOUT\r\n");
        smtp_reply(&mut connection, "* BYE done\r\nD0008 OK logout\r\n").await;
    });

    let results = service
        .search_remote_with_imap_endpoint_for_test(
            &account,
            "from:例@example.test",
            Some("INBOX"),
            10,
            imap_test_endpoint(account.imap_port, true),
        )
        .await
        .unwrap();
    server.await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].account_id, account.id.to_string());
    assert_eq!(results[0].mailbox, "INBOX");
    assert_eq!(results[0].uid, 42);
}

#[tokio::test]
async fn gmail_remote_search_without_extension_does_not_search_all_mail_as_archive() {
    let store = Store::in_memory().await.unwrap();
    let service = MailService::new(store.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
    account.provider_id = "gmail".into();
    account.archive_mailbox = "[Gmail]/All Mail".into();
    account.spam_mailbox = "[Gmail]/Spam".into();
    seed_search_account(
        &store,
        &service,
        &account,
        &[("INBOX", "INBOX", 77), ("Archive", "[Gmail]/All Mail", 88)],
    )
    .await;
    store
        .upsert_catalog_messages(&[
            search_message(&account, "INBOX", 4, "safe inbox result"),
            search_message(&account, "Archive", 40, "must stay unclassified"),
        ])
        .await
        .unwrap();

    let server = tokio::spawn(async move {
        let mut connection = accept_search_client(listener).await;
        assert_eq!(smtp_command(&mut connection).await, "D0003 CAPABILITY\r\n");
        smtp_reply(
            &mut connection,
            "* CAPABILITY IMAP4rev1 X-GM-EXT-10\r\nD0003 OK capability\r\n",
        )
        .await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0004 LIST \"\" \"*\"\r\n"
        );
        smtp_reply(
            &mut connection,
            "* LIST (\\HasNoChildren \\Inbox) \"/\" \"INBOX\"\r\n* LIST (\\HasNoChildren \\All) \"/\" \"[Gmail]/All Mail\"\r\nD0004 OK listed\r\n",
        )
        .await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0005 SELECT \"INBOX\"\r\n"
        );
        smtp_reply(
            &mut connection,
            "* 1 EXISTS\r\n* OK [UIDVALIDITY 77] valid\r\nD0005 OK selected\r\n",
        )
        .await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0006 UID SEARCH TEXT \"result\"\r\n"
        );
        smtp_reply(&mut connection, "* SEARCH 4\r\nD0006 OK searched\r\n").await;
        assert_eq!(smtp_command(&mut connection).await, "D0007 LOGOUT\r\n");
        smtp_reply(&mut connection, "* BYE done\r\nD0007 OK logout\r\n").await;
    });

    let results = service
        .search_remote_with_imap_endpoint_for_test(
            &account,
            "result",
            None,
            10,
            imap_test_endpoint(account.imap_port, true),
        )
        .await
        .unwrap();
    server.await.unwrap();
    assert_eq!(
        results
            .iter()
            .map(|message| (message.mailbox.as_str(), message.uid))
            .collect::<Vec<_>>(),
        vec![("INBOX", 4)]
    );
}

#[tokio::test]
async fn remote_search_badcharset_before_or_after_continuation_preserves_reopened_cache_without_fetching(
) {
    for reject_before_continuation in [true, false] {
        let profile = tempfile::tempdir().unwrap();
        let database = profile.path().join("search.db");
        let store = Store::open(&database).await.unwrap();
        let service = MailService::new(store.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
        seed_search_account(&store, &service, &account, &[("INBOX", "INBOX", 77)]).await;

        let mut committed = search_message(&account, "INBOX", 42, "preserved cache");
        committed.body_text = "durable catalogue body".into();
        committed.content_state = "complete".into();
        let committed_id = committed.id.clone();
        store.upsert_messages(&[committed]).await.unwrap();
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
            .unwrap();
        let baseline = store.message(&committed_id).await.unwrap().unwrap();

        let server = tokio::spawn(async move {
            let mut connection = accept_search_client(listener).await;
            assert_eq!(
                smtp_command(&mut connection).await,
                "D0003 LIST \"\" \"*\"\r\n"
            );
            smtp_reply(
                &mut connection,
                "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\nD0003 OK listed\r\n",
            )
            .await;
            assert_eq!(
                smtp_command(&mut connection).await,
                "D0004 SELECT \"INBOX\"\r\n"
            );
            smtp_reply(
                &mut connection,
                "* 1 EXISTS\r\n* OK [UIDVALIDITY 77] valid\r\nD0004 OK selected\r\n",
            )
            .await;
            if reject_before_continuation {
                assert_eq!(
                    smtp_command(&mut connection).await,
                    "D0005 UID SEARCH CHARSET UTF-8 TEXT {6}\r\n"
                );
            } else {
                accept_utf8_search_literal(&mut connection, "D0005", "雪道").await;
            }
            smtp_reply(
                &mut connection,
                "D0005 NO [BADCHARSET (US-ASCII)] private-provider-diagnostic\r\n",
            )
            .await;
            assert_connection_closes_without_command(&mut connection, "BADCHARSET refusal").await;
        });

        let error = service
            .search_remote_with_imap_endpoint_for_test(
                &account,
                "雪道",
                Some("INBOX"),
                10,
                imap_test_endpoint(account.imap_port, true),
            )
            .await
            .unwrap_err();
        let rendered = error.to_string();
        assert_eq!(rendered, "IMAP server does not support UTF-8 remote search");
        assert!(!rendered.contains("private-provider-diagnostic"));
        server.await.unwrap();

        drop(service);
        drop(store);
        let reopened = Store::open(&database).await.unwrap();
        let preserved = reopened.message(&committed_id).await.unwrap().unwrap();
        assert_eq!(preserved.account_id, account.id.to_string());
        assert_eq!(preserved.mailbox, "INBOX");
        assert_eq!(preserved.uid, 42);
        assert_eq!(preserved.subject, baseline.subject);
        assert_eq!(preserved.body_text, baseline.body_text);
        assert_eq!(preserved.body_html, baseline.body_html);
        assert_eq!(preserved.content_state, baseline.content_state);
        let cached = reopened
            .cached_message_content(&committed_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cached.body_text, "durable cached body");
        assert_eq!(
            cached.body_html.as_deref(),
            Some("<p>durable cached body</p>")
        );
        let state = reopened
            .mailbox_catalog_state(account.id, "INBOX")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.uid_validity, 77);
    }
}

#[tokio::test]
async fn remote_search_rejects_uidvalidity_change_before_search_or_results() {
    let store = Store::in_memory().await.unwrap();
    let service = MailService::new(store.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
    seed_search_account(&store, &service, &account, &[("INBOX", "INBOX", 77)]).await;
    store
        .upsert_catalog_messages(&[search_message(&account, "INBOX", 42, "stale locator")])
        .await
        .unwrap();

    let server = tokio::spawn(async move {
        let mut connection = accept_search_client(listener).await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0003 LIST \"\" \"*\"\r\n"
        );
        smtp_reply(&mut connection, "D0003 OK listed\r\n").await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0004 SELECT \"INBOX\"\r\n"
        );
        smtp_reply(
            &mut connection,
            "* 1 EXISTS\r\n* OK [UIDVALIDITY 78] replaced\r\nD0004 OK selected\r\n",
        )
        .await;
        let mut forbidden = String::new();
        match timeout(
            Duration::from_millis(300),
            connection.read_line(&mut forbidden),
        )
        .await
        {
            Ok(Ok(0)) | Ok(Err(_)) => {}
            Ok(Ok(_)) => panic!("SEARCH followed a stale UIDVALIDITY: {forbidden:?}"),
            Err(_) => panic!("client kept a stale remote-search connection open"),
        }
    });

    let error = service
        .search_remote_with_imap_endpoint_for_test(
            &account,
            "stale",
            Some("INBOX"),
            10,
            imap_test_endpoint(account.imap_port, true),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("mailbox identity changed"));
    server.await.unwrap();
}

#[tokio::test]
async fn cancelling_remote_search_drops_the_provider_connection_without_followup_work() {
    let store = Store::in_memory().await.unwrap();
    let service = MailService::new(store.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
    seed_search_account(&store, &service, &account, &[("INBOX", "INBOX", 77)]).await;
    let endpoint = imap_test_endpoint(account.imap_port, true);
    let (search_seen_tx, search_seen_rx) = oneshot::channel();

    let server = tokio::spawn(async move {
        let mut connection = accept_search_client(listener).await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0003 LIST \"\" \"*\"\r\n"
        );
        smtp_reply(&mut connection, "D0003 OK listed\r\n").await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0004 SELECT \"INBOX\"\r\n"
        );
        smtp_reply(
            &mut connection,
            "* 0 EXISTS\r\n* OK [UIDVALIDITY 77] valid\r\nD0004 OK selected\r\n",
        )
        .await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0005 UID SEARCH TEXT \"cancel me\"\r\n"
        );
        search_seen_tx.send(()).unwrap();
        let mut forbidden = String::new();
        match timeout(Duration::from_secs(1), connection.read_line(&mut forbidden)).await {
            Ok(Ok(0)) | Ok(Err(_)) => {}
            Ok(Ok(_)) => panic!("cancelled search sent follow-up work: {forbidden:?}"),
            Err(_) => panic!("cancellation did not close the provider connection"),
        }
    });

    let task_account = account.clone();
    let task = tokio::spawn(async move {
        service
            .search_remote_with_imap_endpoint_for_test(
                &task_account,
                "cancel me",
                Some("INBOX"),
                10,
                endpoint,
            )
            .await
    });
    timeout(Duration::from_secs(2), search_seen_rx)
        .await
        .expect("provider never received SEARCH")
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    server.await.unwrap();
}

#[tokio::test]
async fn empty_or_zero_limit_remote_search_performs_no_credential_or_network_work() {
    let store = Store::in_memory().await.unwrap();
    let service = MailService::new(store);
    let account = test_account();

    assert!(service
        .search_remote_with_imap_endpoint_for_test(
            &account,
            "  ",
            Some("INBOX"),
            10,
            imap_test_endpoint(9, true),
        )
        .await
        .unwrap()
        .is_empty());
    assert!(service
        .search_remote_with_imap_endpoint_for_test(
            &account,
            "must not connect",
            Some("INBOX"),
            0,
            imap_test_endpoint(9, true),
        )
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn gmail_multiple_folder_representations_move_once_and_reconcile_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("gmail-aliases.sqlite");
    let store = Store::open(&database).await.unwrap();
    let service = MailService::new(store.clone());
    let mut account = test_account();
    account.provider_id = "gmail".into();
    account.archive_mailbox = "[Gmail]/All Mail".into();
    account.spam_mailbox = "[Gmail]/Spam".into();
    store.save_account(&account).await.unwrap();
    for (mailbox, remote, uid_validity) in [
        ("INBOX", "INBOX", 7),
        ("Archive", "[Gmail]/All Mail", 7),
        ("Trash", "[Gmail]/Trash", 900),
    ] {
        store
            .save_mailbox_catalog_state(account.id, mailbox, remote, uid_validity, 1, true)
            .await
            .unwrap();
    }
    let mut inbox = search_message(&account, "INBOX", 4, "one Gmail message");
    let mut archive = search_message(&account, "Archive", 40, "one Gmail message");
    inbox.message_id = Some("<same-gmail-message@example.test>".into());
    archive.message_id = inbox.message_id.clone();
    archive.thread_id = inbox.thread_id.clone();
    store
        .upsert_catalog_messages(&[inbox, archive])
        .await
        .unwrap();

    let (mut client, server_stream) = duplex_imap_client_and_server();
    let server = tokio::spawn(async move {
        let (read, mut write) = split(server_stream);
        let mut read = BufReader::new(read);
        let mut transcript = Vec::new();
        let tag = scripted_expect_command(
            &mut read,
            &mut transcript,
            "LOGIN \"reader@example.test\" \"sync secret\"",
        )
        .await;
        write
            .write_all(format!("{tag} OK authenticated\r\n").as_bytes())
            .await
            .unwrap();
        let tag = scripted_expect_command(&mut read, &mut transcript, "CAPABILITY").await;
        write
            .write_all(
                format!("* CAPABILITY IMAP4rev1 X-GM-EXT-1\r\n{tag} OK capability\r\n").as_bytes(),
            )
            .await
            .unwrap();
        let tag = scripted_expect_command(&mut read, &mut transcript, "LIST \"\" \"*\"").await;
        write
            .write_all(
                format!(
                    "* LIST (\\HasNoChildren \\Inbox) \"/\" \"INBOX\"\r\n* LIST (\\HasNoChildren \\All) \"/\" \"[Gmail]/All Mail\"\r\n* LIST (\\HasNoChildren \\Trash) \"/\" \"[Gmail]/Trash\"\r\n{tag} OK listed\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        for (remote, uid) in [("INBOX", 4), ("[Gmail]/All Mail", 40)] {
            let tag =
                scripted_expect_command(&mut read, &mut transcript, &format!("SELECT {remote:?}"))
                    .await;
            write
                .write_all(
                    format!("* 1 EXISTS\r\n* OK [UIDVALIDITY 7] valid\r\n{tag} OK selected\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
            let tag = scripted_expect_command(
                &mut read,
                &mut transcript,
                "UID SEARCH FROM \"sender@example.test\"",
            )
            .await;
            write
                .write_all(format!("* SEARCH {uid}\r\n{tag} OK searched\r\n").as_bytes())
                .await
                .unwrap();
            let tag = scripted_expect_command(
                &mut read,
                &mut transcript,
                &format!(
                    "UID FETCH {uid} (X-GM-MSGID X-GM-LABELS BODY.PEEK[HEADER.FIELDS (FROM)])"
                ),
            )
            .await;
            let headers = b"From: Sender <sender@example.test>\r\n\r\n";
            write
                .write_all(
                    format!(
                        "* 1 FETCH (UID {uid} X-GM-MSGID 1000000000000000001 X-GM-LABELS (\\Inbox) BODY[HEADER.FIELDS (FROM)] {{{}}}\r\n",
                        headers.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            write.write_all(headers).await.unwrap();
            write
                .write_all(format!(")\r\n{tag} OK fetched\r\n").as_bytes())
                .await
                .unwrap();
        }

        for remote in ["[Gmail]/All Mail", "INBOX"] {
            let tag =
                scripted_expect_command(&mut read, &mut transcript, &format!("SELECT {remote:?}"))
                    .await;
            write
                .write_all(
                    format!("* 1 EXISTS\r\n* OK [UIDVALIDITY 7] valid\r\n{tag} OK selected\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
        }
        let tag =
            scripted_expect_command(&mut read, &mut transcript, "UID MOVE 4 \"[Gmail]/Trash\"")
                .await;
        write
            .write_all(format!("{tag} OK [COPYUID 900 4 99] moved\r\n").as_bytes())
            .await
            .unwrap();
        let tag = scripted_expect_command(&mut read, &mut transcript, "LOGOUT").await;
        write
            .write_all(format!("* BYE done\r\n{tag} OK logout\r\n").as_bytes())
            .await
            .unwrap();
        transcript
    });

    let result = service
        .trash_messages_from_sender_with_client(
            &mut client,
            &account,
            "sync secret",
            "sender@example.test",
        )
        .await
        .unwrap();
    assert_eq!(result.matched, 1, "one X-GM-MSGID is one remote message");
    assert_eq!(result.moved, 1);
    assert_eq!(result.failed, 0);
    let transcript = server.await.unwrap();
    assert_eq!(
        transcript
            .iter()
            .filter(|command| command.starts_with("UID MOVE"))
            .count(),
        1,
        "folder aliases must not produce duplicate remote mutations"
    );

    drop(service);
    drop(store);
    let reopened = Store::open(&database).await.unwrap();
    assert!(reopened
        .message_by_locator(account.id, "INBOX", 4)
        .await
        .unwrap()
        .is_none());
    assert!(reopened
        .message_by_locator(account.id, "Archive", 40)
        .await
        .unwrap()
        .is_none());
    let moved = reopened
        .message_by_locator(account.id, "Trash", 99)
        .await
        .unwrap()
        .expect("one destination row survives restart");
    assert_eq!(moved.subject, "one Gmail message");
    assert_eq!(moved.mailbox_uid_validity, Some(900));
}

async fn run_gmail_full_sync_without_extension(
    service: &MailService,
    account: &Account,
    reset_before_sync: bool,
) -> Vec<String> {
    let (mut client, server_stream) = duplex_imap_client_and_server();
    let server = tokio::spawn(async move {
        let (read, mut write) = split(server_stream);
        let mut read = BufReader::new(read);
        let mut transcript = Vec::new();
        let mut inbox_selects = 0usize;
        loop {
            let (tag, command) = scripted_command(&mut read).await;
            transcript.push(command.clone());
            let response = if command == "LOGIN \"reader@example.test\" \"sync secret\"" {
                format!("{tag} OK authenticated\r\n")
            } else if command == "CAPABILITY" {
                format!("* CAPABILITY IMAP4rev1 X-GM-EXT-10\r\n{tag} OK capability\r\n")
            } else if command == "LIST \"\" \"*\"" {
                format!(
                    "* LIST (\\HasNoChildren \\Inbox) \"/\" \"INBOX\"\r\n* LIST (\\HasNoChildren \\All) \"/\" \"[Gmail]/All Mail\"\r\n{tag} OK listed\r\n"
                )
            } else if command == "SELECT \"INBOX\"" {
                inbox_selects += 1;
                format!("* 1 EXISTS\r\n* OK [UIDVALIDITY 7] valid\r\n* OK [UIDNEXT 5] next\r\n{tag} OK selected\r\n")
            } else if command == "FETCH 1:1 (UID FLAGS)" {
                format!("* 1 FETCH (UID 4 FLAGS ())\r\n{tag} OK snapshot\r\n")
            } else if command.starts_with("UID FETCH 4 (FLAGS INTERNALDATE") {
                let headers = "Date: Wed, 30 Jul 2026 10:00:00 +0000\r\nFrom: Sender <sender@example.test>\r\nTo: Reader <reader@example.test>\r\nSubject: One Gmail message\r\nMessage-ID: <same-gmail-message@example.test>\r\n\r\n";
                format!("* 1 FETCH (UID 4 FLAGS () INTERNALDATE \"30-Jul-2026 10:00:00 +0000\" RFC822.SIZE 4 BODYSTRUCTURE (\"TEXT\" \"PLAIN\" (\"CHARSET\" \"utf-8\") NIL NIL \"7BIT\" 4 1) X-GM-LABELS (\\Inbox) BODY[HEADER] {{{}}}\r\n{}\r\n)\r\n{tag} OK metadata\r\n", headers.len(), headers)
            } else if command == "UID FETCH 4 (BODY.PEEK[]<0.8192>)" {
                format!("* 1 FETCH (UID 4 BODY[]<0> {{4}}\r\nbody)\r\n{tag} OK snippet\r\n")
            } else {
                panic!("unexpected Gmail sync command: {command}")
            };
            write.write_all(response.as_bytes()).await.unwrap();
            if inbox_selects == 3 {
                return transcript;
            }
        }
    });

    service
        .sync_mailboxes_with_progress_on_client(
            &mut client,
            account,
            50,
            vec![
                MailboxPlan::new("INBOX", "INBOX"),
                MailboxPlan::gmail_archive("[Gmail]/All Mail"),
            ],
            reset_before_sync,
            |_| {},
        )
        .await
        .unwrap();
    server.await.unwrap()
}

#[tokio::test]
async fn gmail_full_sync_without_extension_leaves_all_mail_unclassified_and_untouched() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("gmail-sync-aliases.sqlite");
    let store = Store::open(&database).await.unwrap();
    let service = MailService::new(store.clone());
    let mut account = test_account();
    account.provider_id = "gmail".into();
    account.archive_mailbox = "[Gmail]/All Mail".into();
    account.spam_mailbox = "[Gmail]/Spam".into();
    store.save_account(&account).await.unwrap();
    service
        .credentials()
        .set_password(&account, "sync secret")
        .await
        .unwrap();

    let mut archived = search_message(&account, "Archive", 40, "retained cached archive");
    archived.content_state = "complete".into();
    let archived_id = archived.id.clone();
    store.upsert_messages(&[archived]).await.unwrap();
    store
        .save_mailbox_catalog_state(account.id, "Archive", "[Gmail]/All Mail", 8, 1, true)
        .await
        .unwrap();
    store
        .cache_message_content(
            &archived_id,
            false,
            CachedMessageContent {
                body_text: "retained archive body".into(),
                body_html: None,
                unsubscribe_kind: None,
                attachments: Vec::new(),
            },
        )
        .await
        .unwrap();

    for reset_before_sync in [false, true] {
        let transcript =
            run_gmail_full_sync_without_extension(&service, &account, reset_before_sync).await;
        assert!(
            !transcript
                .iter()
                .any(|command| command == "SELECT \"[Gmail]/All Mail\""),
            "without X-GM-LABELS, All Mail must remain untouched rather than be misclassified as Archive"
        );
        assert!(
            !transcript
                .iter()
                .any(|command| command.contains("X-GM-LABELS") || command.contains("X-GM-MSGID")),
            "a missing exact extension must use only generic IMAP metadata"
        );
        assert!(
            store
                .message_by_locator(account.id, "Archive", 40)
                .await
                .unwrap()
                .is_some(),
            "a skipped All Mail view must not prune cached Archive data (reset={reset_before_sync})"
        );
        assert_eq!(
            store
                .cached_message_content(&archived_id)
                .await
                .unwrap()
                .unwrap()
                .body_text,
            "retained archive body",
            "a skipped All Mail view must preserve cached content (reset={reset_before_sync})"
        );
    }
    assert_eq!(
        store.mailbox_uids(account.id, "INBOX").await.unwrap(),
        [4].into()
    );
    assert_eq!(
        store.mailbox_uids(account.id, "Archive").await.unwrap(),
        [40].into()
    );

    drop(service);
    drop(store);
    let reopened = Store::open(&database).await.unwrap();
    assert!(reopened
        .message_by_locator(account.id, "INBOX", 4)
        .await
        .unwrap()
        .is_some());
    assert!(reopened
        .message_by_locator(account.id, "Archive", 40)
        .await
        .unwrap()
        .is_some());
    assert_eq!(
        reopened
            .cached_message_content(&archived_id)
            .await
            .unwrap()
            .unwrap()
            .body_text,
        "retained archive body"
    );
}

#[tokio::test]
async fn gmail_inbox_sent_and_all_mail_sync_converges_to_one_reader_copy() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("gmail-three-label-sync.sqlite");
    let store = Store::open(&database).await.unwrap();
    let service = MailService::new(store.clone());
    let mut account = test_account();
    account.provider_id = "gmail".into();
    account.archive_mailbox = "[Gmail]/All Mail".into();
    account.spam_mailbox = "[Gmail]/Spam".into();
    store.save_account(&account).await.unwrap();
    service
        .credentials()
        .set_password(&account, "sync secret")
        .await
        .unwrap();

    let (mut client, server_stream) = duplex_imap_client_and_server();
    let server = tokio::spawn(async move {
        let (read, mut write) = split(server_stream);
        let mut read = BufReader::new(read);
        let mut transcript = Vec::new();
        let mut selected = String::new();
        let mut selects = HashMap::<String, usize>::new();
        loop {
            let (tag, command) = scripted_command(&mut read).await;
            transcript.push(command.clone());
            let response = if command == "LOGIN \"reader@example.test\" \"sync secret\"" {
                format!("{tag} OK authenticated\r\n")
            } else if command == "CAPABILITY" {
                format!("* CAPABILITY IMAP4rev1 X-GM-EXT-1\r\n{tag} OK capability\r\n")
            } else if command == "LIST \"\" \"*\"" {
                format!("* LIST (\\HasNoChildren \\Inbox) \"/\" \"INBOX\"\r\n* LIST (\\HasNoChildren \\Sent) \"/\" \"[Gmail]/Sent Mail\"\r\n* LIST (\\HasNoChildren \\All) \"/\" \"[Gmail]/All Mail\"\r\n{tag} OK listed\r\n")
            } else if let Some(remote) = command
                .strip_prefix("SELECT ")
                .and_then(|value| value.strip_prefix('"'))
                .and_then(|value| value.strip_suffix('"'))
            {
                selected = remote.to_owned();
                *selects.entry(selected.clone()).or_default() += 1;
                let uid_next = match selected.as_str() {
                    "INBOX" => 5,
                    "[Gmail]/Sent Mail" => 41,
                    "[Gmail]/All Mail" => 401,
                    other => panic!("unexpected Gmail mailbox: {other}"),
                };
                format!("* 1 EXISTS\r\n* OK [UIDVALIDITY 7] valid\r\n* OK [UIDNEXT {uid_next}] next\r\n{tag} OK selected\r\n")
            } else if command == "FETCH 1:1 (UID FLAGS)" {
                let uid = match selected.as_str() {
                    "INBOX" => 4,
                    "[Gmail]/Sent Mail" => 40,
                    "[Gmail]/All Mail" => 400,
                    other => panic!("snapshot in unexpected Gmail mailbox: {other}"),
                };
                format!("* 1 FETCH (UID {uid} FLAGS ())\r\n{tag} OK snapshot\r\n")
            } else if command.starts_with("UID FETCH ") && command.contains("FLAGS INTERNALDATE") {
                let (uid, labels) = match selected.as_str() {
                    "INBOX" => (4, "\\Inbox"),
                    "[Gmail]/Sent Mail" => (40, "\\Inbox \\Sent"),
                    "[Gmail]/All Mail" => (400, "\\Inbox \\Sent"),
                    other => panic!("metadata in unexpected Gmail mailbox: {other}"),
                };
                let headers = match selected.as_str() {
                    "INBOX" => "Date: Wed, 30 Jul 2026 10:00:00 +0000\r\nFrom: Sender <sender@example.test>\r\nTo: Reader <reader@example.test>\r\nSubject: One Gmail message\r\nMessage-ID: <same-gmail-message@example.test>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n",
                    "[Gmail]/Sent Mail" => "Date: Wed, 30 Jul 2026 10:00:00 +0000\r\nFrom: Sender <sender@example.test>\r\nTo: Reader <reader@example.test>\r\nSubject: One Gmail message\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n",
                    "[Gmail]/All Mail" => "Date: Wed, 30 Jul 2026 10:00:00 +0000\r\nFrom: Sender <sender@example.test>\r\nTo: Reader <reader@example.test>\r\nSubject: One Gmail message\r\nMessage-ID: malformed header\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n",
                    other => panic!("metadata in unexpected Gmail mailbox: {other}"),
                };
                format!("* 1 FETCH (UID {uid} FLAGS () INTERNALDATE \"30-Jul-2026 10:00:00 +0000\" RFC822.SIZE 4 BODYSTRUCTURE (\"TEXT\" \"PLAIN\" (\"CHARSET\" \"utf-8\") NIL NIL \"7BIT\" 4 1) X-GM-LABELS (\"X-GM-MSGID 444\" {labels}) x-Gm-MsGiD 9001 BODY[HEADER] {{{}}}\r\n{}\r\n)\r\n{tag} OK metadata\r\n", headers.len(), headers)
            } else if command.starts_with("UID FETCH ") && command.contains("BODY.PEEK[]<0.8192>") {
                assert_ne!(
                    selected, "[Gmail]/All Mail",
                    "All Mail system-labelled copy must not fetch a body"
                );
                let uid = if selected == "INBOX" { 4 } else { 40 };
                format!("* 1 FETCH (UID {uid} BODY[]<0> {{4}}\r\nbody\r\n)\r\n{tag} OK snippet\r\n")
            } else if command == "LOGOUT" {
                format!("* BYE done\r\n{tag} OK logout\r\n")
            } else {
                panic!("unexpected Gmail sync command: {command}")
            };
            write.write_all(response.as_bytes()).await.unwrap();
            if selects.values().all(|count| *count >= 3) && selects.len() == 3 {
                return transcript;
            }
        }
    });

    let sync_result = timeout(
        Duration::from_secs(5),
        service.sync_mailboxes_with_progress_on_client(
            &mut client,
            &account,
            50,
            vec![
                MailboxPlan::new("INBOX", "INBOX"),
                MailboxPlan::new("[Gmail]/Sent Mail", "Sent"),
                MailboxPlan::gmail_archive("[Gmail]/All Mail"),
            ],
            true,
            |_| {},
        ),
    )
    .await;
    match sync_result {
        Ok(result) => result.unwrap(),
        Err(_) => {
            server.abort();
            panic!("Gmail three-label sync exceeded its bounded test timeout");
        }
    };
    let mut server = server;
    let transcript = match timeout(Duration::from_secs(5), &mut server).await {
        Ok(result) => result.unwrap(),
        Err(_) => {
            server.abort();
            panic!("Gmail three-label scripted server exceeded its bounded test timeout");
        }
    };
    assert!(transcript
        .iter()
        .any(|command| command.contains("UID FETCH 40 (FLAGS INTERNALDATE")));
    assert!(transcript
        .iter()
        .any(|command| command.contains("UID FETCH 400 (FLAGS INTERNALDATE")));
    assert!(!transcript
        .iter()
        .any(|command| command.contains("UID FETCH 400 (BODY.PEEK[]<0.8192>)")));

    assert_eq!(
        store.mailbox_uids(account.id, "INBOX").await.unwrap(),
        [4].into()
    );
    assert_eq!(
        store.mailbox_uids(account.id, "Sent").await.unwrap(),
        [40].into()
    );
    let sent = store
        .message_by_locator(account.id, "Sent", 40)
        .await
        .unwrap()
        .unwrap();
    assert!(sent.message_id.is_none());
    assert_eq!(
        store
            .gmail_provider_identity_for_message(&sent.id)
            .await
            .unwrap()
            .as_deref(),
        Some("9001"),
        "a quoted label must not impersonate a mixed-case X-GM-MSGID attribute"
    );
    assert!(store
        .mailbox_uids(account.id, "Archive")
        .await
        .unwrap()
        .is_empty());

    for mailbox in [None, Some("INBOX"), Some("Sent")] {
        let conversations = store
            .search_conversations(&SearchQuery {
                account_ids: vec![account.id],
                mailbox: mailbox.map(str::to_owned),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(conversations.len(), 1, "one conversation for {mailbox:?}");
        assert_eq!(
            conversations[0].messages.len(),
            1,
            "one Reader copy for {mailbox:?}"
        );
        assert_eq!(
            conversations[0].source_messages.len(),
            2,
            "INBOX and Sent locators retained for {mailbox:?}"
        );
        let locators = conversations[0]
            .source_messages
            .iter()
            .map(|message| (message.mailbox.as_str(), message.uid))
            .collect::<HashSet<_>>();
        assert_eq!(
            locators,
            HashSet::from([("INBOX", 4), ("Sent", 40)]),
            "both concrete Gmail locators remain available for {mailbox:?}"
        );
        assert_eq!(
            conversations[0].messages[0].mailbox,
            mailbox.unwrap_or("INBOX"),
            "the selected mailbox representative is stable for {mailbox:?}"
        );
    }

    drop(service);
    drop(store);
    let reopened = Store::open(&database).await.unwrap();
    assert_eq!(
        reopened.mailbox_uids(account.id, "INBOX").await.unwrap(),
        [4].into()
    );
    assert_eq!(
        reopened.mailbox_uids(account.id, "Sent").await.unwrap(),
        [40].into()
    );
    assert!(reopened
        .mailbox_uids(account.id, "Archive")
        .await
        .unwrap()
        .is_empty());
    let conversations = reopened
        .search_conversations(&SearchQuery {
            account_ids: vec![account.id],
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(conversations.len(), 1);
    assert_eq!(conversations[0].messages.len(), 1);
    assert_eq!(conversations[0].source_messages.len(), 2);
    assert_eq!(conversations[0].messages[0].mailbox, "INBOX");
    assert_eq!(
        conversations[0]
            .source_messages
            .iter()
            .map(|message| (message.mailbox.as_str(), message.uid))
            .collect::<HashSet<_>>(),
        HashSet::from([("INBOX", 4), ("Sent", 40)])
    );
    for mailbox in ["INBOX", "Sent"] {
        let conversations = reopened
            .search_conversations(&SearchQuery {
                account_ids: vec![account.id],
                mailbox: Some(mailbox.to_owned()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(conversations.len(), 1);
        assert_eq!(conversations[0].messages.len(), 1);
        assert_eq!(conversations[0].messages[0].mailbox, mailbox);
        assert_eq!(
            conversations[0]
                .source_messages
                .iter()
                .map(|message| (message.mailbox.as_str(), message.uid))
                .collect::<HashSet<_>>(),
            HashSet::from([("INBOX", 4), ("Sent", 40)])
        );
    }
}
