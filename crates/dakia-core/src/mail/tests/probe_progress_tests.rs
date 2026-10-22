//! Fixed, content-free IMAP probe progress regressions.

use super::*;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::TcpListener,
    time::{timeout, Duration},
};

#[tokio::test]
async fn tls_imap_probe_reports_connection_and_read_neutral_progress() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
    let store = Store::in_memory().await.unwrap();
    let service = MailService::new(store);
    service
        .credentials()
        .set_password(&account, "probe secret")
        .await
        .unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = smtp_test_acceptor().accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        smtp_reply(&mut connection, "* OK fictional IMAP ready\r\n").await;
        for (expected, response) in [
            (
                "D0001 CAPABILITY\r\n",
                "* CAPABILITY IMAP4rev1\r\nD0001 OK capability\r\n",
            ),
            (
                "D0002 CAPABILITY\r\n",
                "* CAPABILITY IMAP4rev1\r\nD0002 OK capability\r\n",
            ),
            (
                "D0003 LOGIN \"reader@example.test\" \"probe secret\"\r\n",
                "D0003 OK authenticated\r\n",
            ),
            (
                "D0004 LIST \"\" \"INBOX\"\r\n",
                "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\nD0004 OK listed\r\n",
            ),
            (
                "D0005 EXAMINE \"INBOX\"\r\n",
                "* 1 EXISTS\r\nD0005 OK examined\r\n",
            ),
            (
                "D0006 STATUS \"INBOX\" (UIDVALIDITY UIDNEXT)\r\n",
                "* STATUS \"INBOX\" (UIDVALIDITY 19 UIDNEXT 44)\r\nD0006 OK status\r\n",
            ),
            ("D0007 LOGOUT\r\n", "* BYE done\r\nD0007 OK logout\r\n"),
        ] {
            assert_eq!(smtp_command(&mut connection).await, expected);
            smtp_reply(&mut connection, response).await;
        }
    });
    let mut events = Vec::new();
    let mut observer = |event| {
        events.push(event);
        Ok(())
    };
    let mut client = ImapClient::connect_with_endpoint_and_progress(
        &account,
        imap_test_endpoint(account.imap_port, true),
        &mut observer,
    )
    .await
    .unwrap();
    service
        .imap_auth_probe_with_client_and_progress(&mut client, &account, &mut observer)
        .await
        .unwrap();
    server.await.unwrap();

    assert_eq!(
        events,
        [
            ProbeProgressEvent::Started(ProbeProgressPhase::ImapTcpConnect),
            ProbeProgressEvent::Completed(ProbeProgressPhase::ImapTcpConnect),
            ProbeProgressEvent::Started(ProbeProgressPhase::ImapTlsHandshake),
            ProbeProgressEvent::Completed(ProbeProgressPhase::ImapTlsHandshake),
            ProbeProgressEvent::Started(ProbeProgressPhase::ImapGreeting),
            ProbeProgressEvent::Completed(ProbeProgressPhase::ImapGreeting),
            ProbeProgressEvent::Started(ProbeProgressPhase::ImapPostTlsCapability),
            ProbeProgressEvent::Completed(ProbeProgressPhase::ImapPostTlsCapability),
            ProbeProgressEvent::Started(ProbeProgressPhase::ImapCapability),
            ProbeProgressEvent::Completed(ProbeProgressPhase::ImapCapability),
            ProbeProgressEvent::Started(ProbeProgressPhase::ImapAuthenticate),
            ProbeProgressEvent::Completed(ProbeProgressPhase::ImapAuthenticate),
            ProbeProgressEvent::Started(ProbeProgressPhase::ImapListInbox),
            ProbeProgressEvent::Completed(ProbeProgressPhase::ImapListInbox),
            ProbeProgressEvent::Started(ProbeProgressPhase::ImapExamineInbox),
            ProbeProgressEvent::Completed(ProbeProgressPhase::ImapExamineInbox),
            ProbeProgressEvent::Started(ProbeProgressPhase::ImapInboxStatus),
            ProbeProgressEvent::Completed(ProbeProgressPhase::ImapInboxStatus),
            ProbeProgressEvent::Started(ProbeProgressPhase::ImapLogout),
            ProbeProgressEvent::Completed(ProbeProgressPhase::ImapLogout),
        ]
    );
}

#[tokio::test]
async fn starttls_imap_connector_reports_upgrade_before_tls_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let account = imap_test_account(Security::StartTls, listener.local_addr().unwrap().port());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut plaintext = BufReader::new(stream);
        smtp_reply(&mut plaintext, "* OK fictional IMAP ready\r\n").await;
        assert_eq!(smtp_command(&mut plaintext).await, "D0000 STARTTLS\r\n");
        smtp_reply(&mut plaintext, "D0000 OK begin TLS\r\n").await;
        let stream = smtp_test_acceptor()
            .accept(plaintext.into_inner())
            .await
            .unwrap();
        let mut encrypted = BufReader::new(stream);
        assert_eq!(smtp_command(&mut encrypted).await, "D0001 CAPABILITY\r\n");
        smtp_reply(
            &mut encrypted,
            "* CAPABILITY IMAP4rev1\r\nD0001 OK capability\r\n",
        )
        .await;
    });
    let mut events = Vec::new();
    ImapClient::connect_with_endpoint_and_progress(
        &account,
        imap_test_endpoint(account.imap_port, true),
        &mut |event| {
            events.push(event);
            Ok(())
        },
    )
    .await
    .unwrap();
    server.await.unwrap();

    assert_eq!(
        events,
        [
            ProbeProgressEvent::Started(ProbeProgressPhase::ImapTcpConnect),
            ProbeProgressEvent::Completed(ProbeProgressPhase::ImapTcpConnect),
            ProbeProgressEvent::Started(ProbeProgressPhase::ImapGreeting),
            ProbeProgressEvent::Completed(ProbeProgressPhase::ImapGreeting),
            ProbeProgressEvent::Started(ProbeProgressPhase::ImapStartTls),
            ProbeProgressEvent::Completed(ProbeProgressPhase::ImapStartTls),
            ProbeProgressEvent::Started(ProbeProgressPhase::ImapTlsHandshake),
            ProbeProgressEvent::Completed(ProbeProgressPhase::ImapTlsHandshake),
            ProbeProgressEvent::Started(ProbeProgressPhase::ImapPostTlsCapability),
            ProbeProgressEvent::Completed(ProbeProgressPhase::ImapPostTlsCapability),
        ]
    );
}

#[tokio::test]
async fn smtp_starttls_probe_reports_fixed_events_and_never_submits_an_envelope() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let account = smtp_test_account(Security::StartTls, listener.local_addr().unwrap().port());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut plaintext = BufReader::new(stream);
        smtp_reply(&mut plaintext, "220 localhost ready\r\n").await;
        assert!(smtp_command(&mut plaintext).await.starts_with("EHLO "));
        smtp_reply(&mut plaintext, "250-localhost\r\n250 STARTTLS\r\n").await;
        assert_eq!(smtp_command(&mut plaintext).await, "STARTTLS\r\n");
        smtp_reply(&mut plaintext, "220 2.0.0 start TLS\r\n").await;

        let stream = smtp_test_acceptor()
            .accept(plaintext.into_inner())
            .await
            .unwrap();
        let mut encrypted = BufReader::new(stream);
        for _ in 0..2 {
            assert!(smtp_command(&mut encrypted).await.starts_with("EHLO "));
            smtp_reply(&mut encrypted, "250-localhost\r\n250 AUTH PLAIN\r\n").await;
        }
        assert_eq!(
            smtp_command(&mut encrypted).await,
            "AUTH PLAIN AHNlbmRlckBleGFtcGxlLnRlc3QAc2VjcmV0\r\n"
        );
        smtp_reply(&mut encrypted, "235 2.7.0 authenticated\r\n").await;
        assert_eq!(smtp_command(&mut encrypted).await, "QUIT\r\n");
        smtp_reply(&mut encrypted, "221 2.0.0 bye\r\n").await;
    });
    let mut events = Vec::new();
    smtp_auth_probe_with_smtp_endpoint_and_progress(
        &account,
        "secret",
        smtp_test_endpoint(&account),
        Duration::from_secs(2),
        &mut |event| {
            events.push(event);
            Ok(())
        },
    )
    .await
    .unwrap();
    server.await.unwrap();

    assert_eq!(
        events,
        [
            ProbeProgressEvent::Started(ProbeProgressPhase::SmtpConnect),
            ProbeProgressEvent::Completed(ProbeProgressPhase::SmtpConnect),
            ProbeProgressEvent::Started(ProbeProgressPhase::SmtpStartTls),
            ProbeProgressEvent::Completed(ProbeProgressPhase::SmtpStartTls),
            ProbeProgressEvent::Started(ProbeProgressPhase::SmtpEhlo),
            ProbeProgressEvent::Completed(ProbeProgressPhase::SmtpEhlo),
            ProbeProgressEvent::Started(ProbeProgressPhase::SmtpAuthenticate),
            ProbeProgressEvent::Completed(ProbeProgressPhase::SmtpAuthenticate),
            ProbeProgressEvent::Started(ProbeProgressPhase::SmtpQuit),
            ProbeProgressEvent::Completed(ProbeProgressPhase::SmtpQuit),
        ]
    );
}

#[tokio::test]
async fn smtp_probe_observer_failure_stops_before_authentication_and_closes_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let account = smtp_test_account(Security::Tls, listener.local_addr().unwrap().port());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = smtp_test_acceptor().accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        smtp_reply(&mut connection, "220 localhost ready\r\n").await;
        for _ in 0..2 {
            assert!(smtp_command(&mut connection).await.starts_with("EHLO "));
            smtp_reply(&mut connection, "250-localhost\r\n250 AUTH PLAIN\r\n").await;
        }
        let mut forbidden = String::new();
        match timeout(
            Duration::from_millis(500),
            connection.read_line(&mut forbidden),
        )
        .await
        {
            Ok(Ok(0)) | Ok(Err(_)) => {}
            Ok(Ok(_)) => panic!("unexpected SMTP command after observer failure: {forbidden:?}"),
            Err(_) => panic!("SMTP connection was not closed after observer failure"),
        }
    });
    let mut events = Vec::new();
    let error = smtp_auth_probe_with_smtp_endpoint_and_progress(
        &account,
        "secret",
        smtp_test_endpoint(&account),
        Duration::from_secs(2),
        &mut |event| {
            events.push(event);
            if event == ProbeProgressEvent::Started(ProbeProgressPhase::SmtpAuthenticate) {
                bail!("fixture observer rejected SMTP authentication")
            }
            Ok(())
        },
    )
    .await
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("fixture observer rejected SMTP authentication"));
    assert_eq!(
        events,
        [
            ProbeProgressEvent::Started(ProbeProgressPhase::SmtpConnect),
            ProbeProgressEvent::Completed(ProbeProgressPhase::SmtpConnect),
            ProbeProgressEvent::Started(ProbeProgressPhase::SmtpEhlo),
            ProbeProgressEvent::Completed(ProbeProgressPhase::SmtpEhlo),
            ProbeProgressEvent::Started(ProbeProgressPhase::SmtpAuthenticate),
        ]
    );
    server.await.unwrap();
}

#[tokio::test]
async fn imap_probe_reports_fixed_events_in_command_order() {
    let store = Store::in_memory().await.unwrap();
    let service = MailService::new(store);
    let account = test_account();
    service
        .credentials()
        .set_password(&account, "probe secret")
        .await
        .unwrap();
    let (mut client, server) = plain_imap_client_and_server().await;
    let server = tokio::spawn(scripted_imap_probe_server(server));
    let mut events = Vec::new();

    service
        .imap_auth_probe_with_client_and_progress(&mut client, &account, &mut |event| {
            events.push(event);
            Ok(())
        })
        .await
        .unwrap();

    assert_eq!(
        events,
        [
            ProbeProgressEvent::Started(ProbeProgressPhase::ImapCapability),
            ProbeProgressEvent::Completed(ProbeProgressPhase::ImapCapability),
            ProbeProgressEvent::Started(ProbeProgressPhase::ImapAuthenticate),
            ProbeProgressEvent::Completed(ProbeProgressPhase::ImapAuthenticate),
            ProbeProgressEvent::Started(ProbeProgressPhase::ImapListInbox),
            ProbeProgressEvent::Completed(ProbeProgressPhase::ImapListInbox),
            ProbeProgressEvent::Started(ProbeProgressPhase::ImapExamineInbox),
            ProbeProgressEvent::Completed(ProbeProgressPhase::ImapExamineInbox),
            ProbeProgressEvent::Started(ProbeProgressPhase::ImapInboxStatus),
            ProbeProgressEvent::Completed(ProbeProgressPhase::ImapInboxStatus),
            ProbeProgressEvent::Started(ProbeProgressPhase::ImapLogout),
            ProbeProgressEvent::Completed(ProbeProgressPhase::ImapLogout),
        ]
    );
    assert_eq!(
        server.await.unwrap(),
        [
            "CAPABILITY",
            "LOGIN \"reader@example.test\" \"probe secret\"",
            "LIST \"\" \"INBOX\"",
            "EXAMINE \"INBOX\"",
            "STATUS \"INBOX\" (UIDVALIDITY UIDNEXT)",
            "LOGOUT",
        ]
    );
}

#[tokio::test]
async fn imap_probe_observer_failure_prevents_authentication() {
    let store = Store::in_memory().await.unwrap();
    let service = MailService::new(store);
    let account = test_account();
    service
        .credentials()
        .set_password(&account, "probe secret")
        .await
        .unwrap();
    let (mut client, server) = plain_imap_client_and_server().await;
    let server = tokio::spawn(async move {
        let (read, mut write) = server.into_split();
        let mut read = BufReader::new(read);
        let (tag, command) = scripted_command(&mut read).await;
        assert_eq!(command, "CAPABILITY");
        write
            .write_all(format!("* CAPABILITY IMAP4rev1\r\n{tag} OK capability\r\n").as_bytes())
            .await
            .unwrap();
        write.flush().await.unwrap();

        let mut forbidden = String::new();
        match timeout(Duration::from_millis(200), read.read_line(&mut forbidden)).await {
            Ok(Ok(0)) | Err(_) | Ok(Err(_)) => {}
            Ok(Ok(_)) => panic!("unexpected authentication after observer failure: {forbidden:?}"),
        }
    });
    let mut events = Vec::new();

    let error = service
        .imap_auth_probe_with_client_and_progress(&mut client, &account, &mut |event| {
            events.push(event);
            if event == ProbeProgressEvent::Started(ProbeProgressPhase::ImapAuthenticate) {
                bail!("fixture observer rejected authentication")
            }
            Ok(())
        })
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("fixture observer rejected authentication"));
    assert_eq!(
        events,
        [
            ProbeProgressEvent::Started(ProbeProgressPhase::ImapCapability),
            ProbeProgressEvent::Completed(ProbeProgressPhase::ImapCapability),
            ProbeProgressEvent::Started(ProbeProgressPhase::ImapAuthenticate),
        ]
    );
    drop(client);
    server.await.unwrap();
}
