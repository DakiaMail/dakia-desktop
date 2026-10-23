use super::*;
use tokio::{io::BufReader, net::TcpListener, time::Duration};

const RAW: &[u8] = b"From: sender@example.test\r\nTo: recipient@example.test\r\nSubject: SIZE\r\n\r\nfictional\r\n";

async fn smtp_size_ehlo<S>(connection: &mut BufReader<S>, line: &str)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    assert!(smtp_command(connection).await.starts_with("EHLO "));
    smtp_reply(
        connection,
        &format!("250-localhost\r\n250-{line}\r\n250 AUTH PLAIN\r\n"),
    )
    .await;
}

#[tokio::test]
async fn rfc1870_below_and_exact_limits_submit_one_size_annotated_transaction() {
    let wire_size = RAW.len() + 2;
    for maximum in [wire_size + 1, wire_size] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let account = smtp_test_account(Security::Tls, listener.local_addr().unwrap().port());
        let acceptor = smtp_test_acceptor();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let stream = acceptor.accept(stream).await.unwrap();
            let mut connection = BufReader::new(stream);
            smtp_reply(&mut connection, "220 localhost ready\r\n").await;
            smtp_size_ehlo(&mut connection, &format!("SIZE {maximum}")).await;
            smtp_size_ehlo(&mut connection, &format!("SIZE {maximum}")).await;
            assert_eq!(
                smtp_command(&mut connection).await,
                "AUTH PLAIN AHNlbmRlckBleGFtcGxlLnRlc3QAc2VjcmV0\r\n"
            );
            smtp_reply(&mut connection, "235 2.7.0 authenticated\r\n").await;
            assert_eq!(
                smtp_command(&mut connection).await,
                format!("MAIL FROM:<sender@example.test> SIZE={wire_size}\r\n")
            );
            smtp_reply(&mut connection, "250 2.1.0 sender accepted\r\n").await;
            assert_eq!(
                smtp_command(&mut connection).await,
                "RCPT TO:<recipient@example.test>\r\n"
            );
            smtp_reply(&mut connection, "250 2.1.5 recipient accepted\r\n").await;
            assert_eq!(smtp_command(&mut connection).await, "DATA\r\n");
            smtp_reply(&mut connection, "354 send message\r\n").await;
            let message = smtp_read_message(&mut connection).await;
            assert_eq!(message.as_bytes(), [RAW, b"\r\n"].concat());
            smtp_reply(&mut connection, "250 2.0.0 queued\r\n").await;
        });

        send_smtp_raw(
            &account,
            &smtp_test_envelope(),
            RAW,
            "secret",
            smtp_test_endpoint(&account),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn rfc1870_above_limit_fails_before_credentials_or_envelope() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let account = smtp_test_account(Security::Tls, listener.local_addr().unwrap().port());
    let acceptor = smtp_test_acceptor();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = acceptor.accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        smtp_reply(&mut connection, "220 localhost ready\r\n").await;
        smtp_size_ehlo(&mut connection, &format!("SIZE {}", RAW.len() + 1)).await;
        smtp_size_ehlo(&mut connection, &format!("SIZE {}", RAW.len() + 1)).await;
        let mut forbidden = String::new();
        match timeout(
            Duration::from_millis(200),
            connection.read_line(&mut forbidden),
        )
        .await
        {
            Ok(Ok(0)) | Err(_) => {}
            Ok(Ok(_)) => panic!("oversized message sent a forbidden SMTP command: {forbidden:?}"),
            Ok(Err(_)) => {}
        }
    });

    let error = send_smtp_raw(
        &account,
        &smtp_test_envelope(),
        RAW,
        "secret",
        smtp_test_endpoint(&account),
        Duration::from_secs(1),
    )
    .await
    .unwrap_err();
    server.await.unwrap();
    let failure = error.downcast_ref::<SendFailure>().unwrap();
    assert_eq!(failure.kind, SendFailureKind::NotSubmitted);
    assert_eq!(
        failure.message,
        "Dakia could not submit this message. It was not sent."
    );
}

#[tokio::test]
async fn rfc1870_zero_size_means_no_fixed_maximum() {
    let wire_size = RAW.len() + 2;
    for advertised in ["SIZE 0", "SIZE 0000"] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let account = smtp_test_account(Security::Tls, listener.local_addr().unwrap().port());
        let acceptor = smtp_test_acceptor();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let stream = acceptor.accept(stream).await.unwrap();
            let mut connection = BufReader::new(stream);
            smtp_reply(&mut connection, "220 localhost ready\r\n").await;
            smtp_size_ehlo(&mut connection, advertised).await;
            smtp_size_ehlo(&mut connection, advertised).await;
            assert_eq!(
                smtp_command(&mut connection).await,
                "AUTH PLAIN AHNlbmRlckBleGFtcGxlLnRlc3QAc2VjcmV0\r\n"
            );
            smtp_reply(&mut connection, "235 2.7.0 authenticated\r\n").await;
            assert_eq!(
                smtp_command(&mut connection).await,
                format!("MAIL FROM:<sender@example.test> SIZE={wire_size}\r\n")
            );
            smtp_reply(&mut connection, "250 2.1.0 sender accepted\r\n").await;
            assert_eq!(
                smtp_command(&mut connection).await,
                "RCPT TO:<recipient@example.test>\r\n"
            );
            smtp_reply(&mut connection, "250 2.1.5 recipient accepted\r\n").await;
            assert_eq!(smtp_command(&mut connection).await, "DATA\r\n");
            smtp_reply(&mut connection, "354 send message\r\n").await;
            assert_eq!(
                smtp_read_message(&mut connection).await.as_bytes(),
                [RAW, b"\r\n"].concat()
            );
            smtp_reply(&mut connection, "250 2.0.0 queued\r\n").await;
        });

        send_smtp_raw(
            &account,
            &smtp_test_envelope(),
            RAW,
            "secret",
            smtp_test_endpoint(&account),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        server.await.unwrap();
    }
}

#[test]
fn rfc1870_size_parser_handles_unbounded_case_and_malformed_values() {
    assert_eq!(
        parse_smtp_size_capability(["localhost", "SIZE"]).unwrap(),
        Some(None)
    );
    assert_eq!(
        parse_smtp_size_capability(["localhost", "SIZE 0"]).unwrap(),
        Some(None)
    );
    assert_eq!(
        parse_smtp_size_capability(["localhost", "SIZE 0000"]).unwrap(),
        Some(None)
    );
    assert_eq!(
        parse_smtp_size_capability(["localhost", "size 42"]).unwrap(),
        Some(Some(42))
    );
    assert_eq!(parse_smtp_size_capability(["localhost"]).unwrap(), None);
    for lines in [
        vec!["localhost", "SIZE nope"],
        vec!["localhost", "SIZE 1 extra"],
        vec!["localhost", "SIZE 1", "SIZE 1"],
        vec!["localhost", "SIZE 999999999999999999999999999999"],
    ] {
        assert!(parse_smtp_size_capability(lines).is_err());
    }
}
