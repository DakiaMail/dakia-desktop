//! Focused protocol and credential regressions for the production mail paths.
//!
//! This module is included inside `mail::tests` so it can drive the same
//! private transport seams as the existing transcript tests. The servers are
//! loopback-only and use the checked-in test certificate fixture.

use super::*;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    time::{timeout, Duration},
};

#[tokio::test]
async fn credentials_are_isolated_for_accounts_with_the_same_username() {
    let store = Store::in_memory().await.unwrap();
    let service = MailService::new(store);
    let mut first = test_account();
    let mut second = test_account();
    first.id = uuid::Uuid::from_u128(1);
    second.id = uuid::Uuid::from_u128(2);
    first.auth = AccountAuth::Password {
        username: "shared@example.test".into(),
    };
    second.auth = AccountAuth::Password {
        username: "shared@example.test".into(),
    };

    service
        .credentials()
        .set_password(&first, "first-secret")
        .await
        .unwrap();
    service
        .credentials()
        .set_password(&second, "second-secret")
        .await
        .unwrap();

    assert_eq!(
        service.credentials().secret(&first).await.unwrap(),
        "first-secret"
    );
    assert_eq!(
        service.credentials().secret(&second).await.unwrap(),
        "second-secret"
    );

    service.credentials().delete(&first).await.unwrap();
    assert!(service.credentials().secret(&first).await.is_err());
    assert_eq!(
        service.credentials().secret(&second).await.unwrap(),
        "second-secret"
    );
}

#[tokio::test]
async fn starttls_rechecks_capabilities_before_authentication() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let account = smtp_test_account(Security::StartTls, listener.local_addr().unwrap().port());
    let acceptor = smtp_test_acceptor();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut plaintext = BufReader::new(stream);
        smtp_reply(&mut plaintext, "220 localhost ready\r\n").await;
        assert!(smtp_command(&mut plaintext).await.starts_with("EHLO "));
        // AUTH is intentionally absent until the connection is encrypted.
        smtp_reply(&mut plaintext, "250-localhost\r\n250 STARTTLS\r\n").await;
        assert_eq!(smtp_command(&mut plaintext).await, "STARTTLS\r\n");
        smtp_reply(&mut plaintext, "220 2.0.0 start TLS\r\n").await;

        let stream = acceptor.accept(plaintext.into_inner()).await.unwrap();
        let mut encrypted = BufReader::new(stream);
        // Lettre performs the mandatory post-STARTTLS EHLO while upgrading,
        // then Dakia deliberately repeats it immediately before AUTH so the
        // submission path can inspect extensions such as RFC 1870 SIZE. Both
        // must observe the encrypted capability set, never the plaintext one.
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

    smtp_auth_probe_with_smtp_endpoint(
        &account,
        "secret",
        smtp_test_endpoint(&account),
        Duration::from_secs(2),
    )
    .await
    .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn each_recipient_rejection_stops_before_data_and_later_recipients() {
    for rejected_index in 0..3 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let account = smtp_test_account(Security::Tls, listener.local_addr().unwrap().port());
        let acceptor = smtp_test_acceptor();
        let recipients = [
            "first@example.test",
            "middle@example.test",
            "last@example.test",
        ];
        let expected_recipients = recipients.map(str::to_owned);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let stream = acceptor.accept(stream).await.unwrap();
            let mut connection = BufReader::new(stream);
            smtp_reply(&mut connection, "220 localhost ready\r\n").await;
            smtp_expect_ehlo_and_auth(&mut connection, false).await;
            assert_eq!(
                smtp_command(&mut connection).await,
                "MAIL FROM:<sender@example.test>\r\n"
            );
            smtp_reply(&mut connection, "250 2.1.0 sender accepted\r\n").await;

            for (index, recipient) in expected_recipients.iter().enumerate() {
                if index > rejected_index {
                    break;
                }
                assert_eq!(
                    smtp_command(&mut connection).await,
                    format!("RCPT TO:<{recipient}>\r\n")
                );
                if index == rejected_index {
                    smtp_reply(&mut connection, "550 5.1.1 rejected fixture recipient\r\n").await;
                } else {
                    smtp_reply(&mut connection, "250 2.1.5 recipient accepted\r\n").await;
                }
            }

            // A rejected RCPT must terminate the transaction. Any DATA or
            // later RCPT would be an unintended partial submission.
            let mut command = String::new();
            match timeout(
                Duration::from_millis(250),
                connection.read_line(&mut command),
            )
            .await
            {
                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => {}
                Ok(Ok(_)) => panic!("unexpected command after recipient rejection: {command:?}"),
            }
        });

        let envelope = Envelope::new(
            Some("sender@example.test".parse().unwrap()),
            recipients
                .iter()
                .map(|value| value.parse().unwrap())
                .collect(),
        )
        .unwrap();
        let error = send_smtp_raw(
            &account,
            &envelope,
            b"From: sender@example.test\r\nTo: recipients@example.test\r\n\r\nfixture\r\n",
            "secret",
            smtp_test_endpoint(&account),
            Duration::from_secs(2),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<SendFailure>().unwrap().kind,
            SendFailureKind::NotSubmitted,
            "recipient position {rejected_index} must remain not submitted"
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn wrong_only_uid_or_body_section_is_not_returned_as_requested_content() {
    for (returned_uid, returned_section) in [(7, "BODY[1]"), (42, "BODY[2]")] {
        let (mut client, server) = plain_imap_client_and_server().await;
        let server = tokio::spawn(async move {
            let (read, mut write) = server.into_split();
            let mut read = BufReader::new(read);
            let (tag, command) = scripted_command(&mut read).await;
            assert_eq!(command, "UID FETCH 42 (BODY.PEEK[1])");
            let response = format!(
                "* 1 FETCH (UID {returned_uid} {returned_section} {{9}}\r\nwrongdata)\r\n{tag} OK FETCH complete\r\n"
            );
            write.write_all(response.as_bytes()).await.unwrap();
            write.flush().await.unwrap();
        });

        let mut result = fetch_uid_data_batched(&mut client, &[42], "BODY.PEEK[1]", 4096).await;
        server.await.unwrap();
        assert!(result.fatal.is_none());
        assert_eq!(result.responses.len(), 1);
        let (_, mut response) = result.responses.pop().unwrap();
        assert!(response.take_body_literal_for(42, "1").is_none());
    }
}
