//! Standards-facing mail safety regressions.
//!
//! These tests exercise public or crate-visible production entry points. They
//! intentionally live outside `mail.rs` so the conformance cases remain easy
//! to audit against the mail-safety catalogue.

use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use lettre::address::Envelope;
use std::sync::Arc;
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::oneshot,
    time::{timeout, Duration},
};
use tokio_rustls::{
    rustls::{
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
        ServerConfig,
    },
    TlsAcceptor,
};

// A public test-only CA and localhost leaf. The corresponding private key is
// deliberately checked in because this identity is valid only for isolated
// loopback transcripts. Production trust stores never include this CA.
const TEST_CA_DER_BASE64: &str = "MIIBoDCCAUWgAwIBAgIUQltW8tjmRLr4QjHNjnIXOGd4oqcwCgYIKoZIzj0EAwIwHTEbMBkGA1UEAwwSRGFraWEgU01UUCB0ZXN0IENBMB4XDTI2MDczMDE2MDE1NFoXDTM2MDcyNzE2MDE1NFowHTEbMBkGA1UEAwwSRGFraWEgU01UUCB0ZXN0IENBMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEowlFdKeMeRMDaJroLiqhOMAQ1dKYMuoX/SXdgSSY0fIcL7K4mv7z8Xqg5iLrw84NQxGZt36GLxNaGfSLmCR6nqNjMGEwHQYDVR0OBBYEFKzJ36GY9x0+2bor86BZVX+U3mOLMB8GA1UdIwQYMBaAFKzJ36GY9x0+2bor86BZVX+U3mOLMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQDAgKEMAoGCCqGSM49BAMCA0kAMEYCIQD5sjoNPPW9m+gCspyKyj9AOdgwZiavQhgDeIvu5hzVgQIhALJpuju+3/idyBTJ1qGomBG4aRuIO9cHhLwuMtAVtMvt";
const TEST_CERT_DER_BASE64: &str = "MIIBxjCCAWygAwIBAgIUM95kwE13FWtKVQEkqO3f8DeMFAgwCgYIKoZIzj0EAwIwHTEbMBkGA1UEAwwSRGFraWEgU01UUCB0ZXN0IENBMB4XDTI2MDczMDE2MDE1NFoXDTM2MDcyNzE2MDE1NFowFDESMBAGA1UEAwwJbG9jYWxob3N0MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAElFRQ2c4qt7037iUsrzdKiDrS/euRkQ3z5uCpfrYsFVhe3g4ffc5IBLZDWSEUP0EJvyEOOg5KL1by1ZGYC/d+S6OBkjCBjzAMBgNVHRMBAf8EAjAAMA4GA1UdDwEB/wQEAwIHgDATBgNVHSUEDDAKBggrBgEFBQcDATAaBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwHQYDVR0OBBYEFM3UTgErLg6p5+pv13yFNths9Lb9MB8GA1UdIwQYMBaAFKzJ36GY9x0+2bor86BZVX+U3mOLMAoGCCqGSM49BAMCA0gAMEUCIEmgwMiWttP7OvYXRkvPm/5c64vpxLLtT+Jg6E4g+OnYAiEAp742aGep2AEwIRP9YXI8RLjhaseLGUQT7R4AWFwnYZE=";
const TEST_KEY_DER_BASE64: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg8ZClgJ8kAl4AVnA0D9d0PXx2siCJiOmjud/vD1NKSqehRANCAASUVFDZziq3vTfuJSyvN0qIOtL965GRDfPm4Kl+tiwVWF7eDh99zkgEtkNZIRQ/QQm/IQ46DkovVvLVkZgL935L";

fn test_account() -> Account {
    Account {
        id: uuid::Uuid::new_v4(),
        email: "sender@example.test".into(),
        account_name: "Standards fixture".into(),
        display_name: "Standards Sender".into(),
        provider_id: "fixture".into(),
        auth: AccountAuth::Password {
            username: "sender@example.test".into(),
        },
        imap_host: "127.0.0.1".into(),
        imap_port: 1,
        imap_security: Security::Tls,
        smtp_host: "127.0.0.1".into(),
        smtp_port: 1,
        smtp_security: Security::Tls,
        archive_mailbox: "Archive".into(),
        spam_mailbox: "Spam".into(),
        enabled: true,
        created_at: Utc::now(),
    }
}

fn test_acceptor() -> TlsAcceptor {
    let certificate = CertificateDer::from(STANDARD.decode(TEST_CERT_DER_BASE64).unwrap());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        STANDARD.decode(TEST_KEY_DER_BASE64).unwrap(),
    ));
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)
        .unwrap();
    TlsAcceptor::from(Arc::new(config))
}

async fn read_line<S: AsyncBufRead + Unpin>(connection: &mut S) -> String {
    let mut line = String::new();
    assert_ne!(connection.read_line(&mut line).await.unwrap(), 0);
    line
}

async fn reply<S: AsyncWrite + Unpin>(connection: &mut S, response: &str) {
    connection.write_all(response.as_bytes()).await.unwrap();
    connection.flush().await.unwrap();
}

async fn greet_and_auth<S>(connection: &mut BufReader<S>, capabilities: &[&str])
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    reply(connection.get_mut(), "220 localhost standards fixture\r\n").await;
    for _ in 0..2 {
        assert!(read_line(connection).await.starts_with("EHLO "));
        let mut response = "250-localhost\r\n250-AUTH PLAIN\r\n".to_owned();
        for (index, capability) in capabilities.iter().enumerate() {
            let separator = if index + 1 == capabilities.len() {
                ' '
            } else {
                '-'
            };
            response.push_str(&format!("250{separator}{capability}\r\n"));
        }
        reply(connection.get_mut(), &response).await;
    }
    assert!(read_line(connection).await.starts_with("AUTH PLAIN "));
    reply(connection.get_mut(), "235 2.7.0 authenticated\r\n").await;
}

fn smtp_endpoint(port: u16) -> SmtpEndpoint {
    MailService::smtp_loopback_endpoint(
        "127.0.0.1".into(),
        port,
        "127.0.0.1".into(),
        STANDARD.decode(TEST_CA_DER_BASE64).unwrap(),
    )
    .unwrap()
}

#[test]
fn smtp_production_phase_timeouts_follow_rfc_recommended_intervals() {
    let timeouts = SmtpPhaseTimeouts::PRODUCTION;
    assert_eq!(timeouts.connect, Duration::from_secs(60));
    assert_eq!(timeouts.tls_handshake, Duration::from_secs(60));
    assert_eq!(timeouts.greeting, Duration::from_secs(5 * 60));
    assert_eq!(timeouts.ehlo, Duration::from_secs(5 * 60));
    assert_eq!(timeouts.starttls_command, Duration::from_secs(2 * 60));
    assert_eq!(timeouts.auth_command, Duration::from_secs(5 * 60));
    assert_eq!(timeouts.mail_command, Duration::from_secs(5 * 60));
    assert_eq!(timeouts.recipient_command, Duration::from_secs(5 * 60));
    assert_eq!(timeouts.data_command, Duration::from_secs(2 * 60));
    assert_eq!(timeouts.data_progress, Duration::from_secs(3 * 60));
    assert_eq!(timeouts.data_terminator, Duration::from_secs(3 * 60));
    assert_eq!(timeouts.final_response, Duration::from_secs(10 * 60));
}

#[tokio::test]
async fn smtp_production_send_path_uses_the_production_phase_policy_once() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let acceptor = test_acceptor();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = acceptor.accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        greet_and_auth(&mut connection, &["8BITMIME"]).await;

        assert_eq!(
            read_line(&mut connection).await,
            "MAIL FROM:<sender@example.test>\r\n"
        );
        // This exceeds the one-second uniform timeout used by the deterministic
        // protocol tests. The production path must retain its RFC phase budget.
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        reply(connection.get_mut(), "250 sender accepted\r\n").await;
        assert_eq!(
            read_line(&mut connection).await,
            "RCPT TO:<recipient@example.test>\r\n"
        );
        reply(connection.get_mut(), "250 recipient accepted\r\n").await;
        assert_eq!(read_line(&mut connection).await, "DATA\r\n");
        reply(connection.get_mut(), "354 continue\r\n").await;
        loop {
            if read_line(&mut connection).await == ".\r\n" {
                break;
            }
        }
        reply(connection.get_mut(), "250 queued once\r\n").await;

        timeout(Duration::from_millis(300), listener.accept())
            .await
            .is_ok()
    });

    let mut account = test_account();
    account.smtp_host = "smtp.gmail.com".into();
    account.smtp_port = port;
    let service = MailService::new(Store::in_memory().await.unwrap());
    let response = service
        .send_with_production_endpoint(
            &account,
            &basic_draft(&account),
            "secret",
            MailSendEndpoints {
                smtp: smtp_endpoint(port),
                sent_imap: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(response, "queued once");
    assert!(
        !server.await.unwrap(),
        "production send opened a replacement SMTP connection"
    );
}

fn imap_endpoint(port: u16, server_name: &str) -> ImapEndpoint {
    MailService::imap_loopback_endpoint(
        "127.0.0.1".into(),
        port,
        server_name.into(),
        STANDARD.decode(TEST_CA_DER_BASE64).unwrap(),
    )
    .unwrap()
}

async fn seed_mailbox_epoch(port: u16) -> (MailService, Account, ImapEndpoint) {
    let store = Store::in_memory().await.unwrap();
    let mut account = test_account();
    account.imap_port = port;
    store.save_account(&account).await.unwrap();
    let service = MailService::new(store.clone());
    service
        .credentials()
        .set_password(&account, "epoch secret")
        .await
        .unwrap();
    let generation = store
        .begin_mailbox_snapshot(
            account.id,
            "INBOX",
            MailboxSnapshotIdentity::new("INBOX", 77, 0, Some(1), None),
        )
        .await
        .unwrap();
    store
        .finalize_mailbox_snapshot(account.id, "INBOX", &generation)
        .await
        .unwrap();
    (service, account, imap_endpoint(port, "127.0.0.1"))
}

async fn epoch_mismatch_server(listener: TcpListener) -> Vec<String> {
    let (stream, _) = listener.accept().await.unwrap();
    let stream = test_acceptor().accept(stream).await.unwrap();
    let mut connection = BufReader::new(stream);
    reply(connection.get_mut(), "* OK loopback IMAP ready\r\n").await;

    let mut transcript = Vec::new();
    let capability = read_line(&mut connection).await;
    let (capability_tag, capability_command) = capability.trim_end().split_once(' ').unwrap();
    transcript.push(capability_command.to_owned());
    assert_eq!(capability_command, "CAPABILITY");
    reply(
        connection.get_mut(),
        &format!("* CAPABILITY IMAP4rev1\r\n{capability_tag} OK capability\r\n"),
    )
    .await;

    let login = read_line(&mut connection).await;
    let (login_tag, login_command) = login.trim_end().split_once(' ').unwrap();
    transcript.push(login_command.to_owned());
    assert_eq!(
        login_command,
        "LOGIN \"sender@example.test\" \"epoch secret\""
    );
    reply(
        connection.get_mut(),
        &format!("{login_tag} OK authenticated\r\n"),
    )
    .await;

    let select = read_line(&mut connection).await;
    let (select_tag, select_command) = select.trim_end().split_once(' ').unwrap();
    transcript.push(select_command.to_owned());
    assert_eq!(select_command, "SELECT \"INBOX\"");
    reply(
        connection.get_mut(),
        &format!(
            "* 0 EXISTS\r\n* OK [UIDVALIDITY 78] changed namespace\r\n{select_tag} OK selected\r\n"
        ),
    )
    .await;

    let mut unexpected = String::new();
    let read = timeout(
        Duration::from_secs(1),
        connection.read_line(&mut unexpected),
    )
    .await;
    assert!(
        unexpected.is_empty(),
        "UIDVALIDITY mismatch allowed an unexpected IMAP command: {unexpected:?}; read result: {read:?}"
    );
    transcript
}

#[tokio::test]
async fn attachment_fetch_rejects_remote_uidvalidity_change_before_any_uid_fetch() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(epoch_mismatch_server(listener));
    let (service, account, endpoint) = seed_mailbox_epoch(port).await;

    let error = service
        .fetch_attachment_at_epoch_with_endpoint_for_test(
            &account,
            "INBOX",
            77,
            "attachment-id-from-old-epoch",
            77,
            endpoint,
        )
        .await
        .expect_err("remote UIDVALIDITY 78 must invalidate attachment UID 77");
    assert!(
        error.to_string().contains("mailbox identity changed"),
        "{error:#}"
    );
    let transcript = server.await.unwrap();
    assert_eq!(
        transcript,
        vec![
            "CAPABILITY",
            "LOGIN \"sender@example.test\" \"epoch secret\"",
            "SELECT \"INBOX\"",
        ]
    );
    assert!(transcript
        .iter()
        .all(|command| !command.contains("UID FETCH")));
}

#[tokio::test]
async fn raw_export_rejects_remote_uidvalidity_change_before_any_uid_fetch() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(epoch_mismatch_server(listener));
    let (service, account, endpoint) = seed_mailbox_epoch(port).await;

    let error = service
        .fetch_raw_message_at_epoch_with_endpoint_for_test(&account, "INBOX", 77, 77, endpoint)
        .await
        .expect_err("remote UIDVALIDITY 78 must invalidate raw export UID 77");
    assert!(
        error.to_string().contains("mailbox identity changed"),
        "{error:#}"
    );
    let transcript = server.await.unwrap();
    assert_eq!(
        transcript,
        vec![
            "CAPABILITY",
            "LOGIN \"sender@example.test\" \"epoch secret\"",
            "SELECT \"INBOX\"",
        ]
    );
    assert!(transcript
        .iter()
        .all(|command| !command.contains("UID FETCH")));
}

#[tokio::test]
async fn smtp_extensions_are_required_before_non_ascii_mail_is_submitted() {
    struct Case {
        name: &'static str,
        capabilities: &'static [&'static str],
        envelope_from: &'static str,
        raw: &'static [u8],
    }

    let cases = [
        Case {
            name: "SMTPUTF8 absent",
            capabilities: &["8BITMIME"],
            envelope_from: "s\u{00f8}ren@example.test",
            raw: b"From: sender@example.test\r\n\r\nASCII body\r\n",
        },
        Case {
            name: "8BITMIME absent",
            capabilities: &["SMTPUTF8"],
            envelope_from: "sender@example.test",
            raw: b"From: sender@example.test\r\n\r\nUTF-8 body: \xc3\xa9\r\n",
        },
    ];

    for case in cases {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let acceptor = test_acceptor();
        let capabilities = case.capabilities;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let stream = acceptor.accept(stream).await.unwrap();
            let mut connection = BufReader::new(stream);
            greet_and_auth(&mut connection, capabilities).await;
            let mut next_command = String::new();
            let next = timeout(
                Duration::from_secs(1),
                connection.read_line(&mut next_command),
            )
            .await;
            assert!(
                next_command.is_empty(),
                "the client sent an SMTP transaction despite a missing capability: {next_command:?}; read result: {next:?}"
            );
        });

        let mut account = test_account();
        account.smtp_port = port;
        let envelope = Envelope::new(
            Some(case.envelope_from.parse().unwrap()),
            vec!["recipient@example.test".parse().unwrap()],
        )
        .unwrap();
        let error = send_smtp_raw(
            &account,
            &envelope,
            case.raw,
            "secret",
            smtp_endpoint(port),
            Duration::from_secs(2),
        )
        .await
        .expect_err(case.name);
        assert_eq!(
            error.downcast_ref::<SendFailure>().unwrap().kind,
            SendFailureKind::NotSubmitted,
            "{}",
            case.name
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn smtp_waits_for_data_permission_before_uploading_any_message_bytes() {
    for data_response in ["354 continue\r\n", "554 5.5.1 DATA rejected\r\n"] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let acceptor = test_acceptor();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let stream = acceptor.accept(stream).await.unwrap();
            let mut connection = BufReader::new(stream);
            greet_and_auth(&mut connection, &["8BITMIME"]).await;

            assert_eq!(
                read_line(&mut connection).await,
                "MAIL FROM:<sender@example.test>\r\n"
            );
            reply(connection.get_mut(), "250 sender accepted\r\n").await;
            assert_eq!(
                read_line(&mut connection).await,
                "RCPT TO:<recipient@example.test>\r\n"
            );
            reply(connection.get_mut(), "250 recipient accepted\r\n").await;
            assert_eq!(read_line(&mut connection).await, "DATA\r\n");

            assert!(
                connection.buffer().is_empty(),
                "message bytes were pipelined in the same read as DATA: {:?}",
                String::from_utf8_lossy(connection.buffer())
            );
            let early_bytes = timeout(Duration::from_millis(200), connection.fill_buf()).await;
            assert!(
                early_bytes.is_err(),
                "message bytes arrived before DATA permission: {early_bytes:?}"
            );

            reply(connection.get_mut(), data_response).await;
            if data_response.starts_with("354") {
                let mut wire = String::new();
                loop {
                    let line = read_line(&mut connection).await;
                    if line == ".\r\n" {
                        break;
                    }
                    wire.push_str(&line);
                }
                assert!(wire.contains("DATA permission barrier fixture"));
                reply(connection.get_mut(), "250 queued\r\n").await;
            } else {
                let mut forbidden = String::new();
                let read = timeout(
                    Duration::from_millis(200),
                    connection.read_line(&mut forbidden),
                )
                .await;
                assert!(
                    forbidden.is_empty(),
                    "message bytes followed a rejected DATA command: {forbidden:?}; read result: {read:?}"
                );
            }
        });

        let mut account = test_account();
        account.smtp_port = port;
        let envelope = Envelope::new(
            Some("sender@example.test".parse().unwrap()),
            vec!["recipient@example.test".parse().unwrap()],
        )
        .unwrap();
        let result = send_smtp_raw(
            &account,
            &envelope,
            b"From: sender@example.test\r\nTo: recipient@example.test\r\nSubject: DATA permission barrier fixture\r\n\r\nbody\r\n",
            "secret",
            smtp_endpoint(port),
            Duration::from_secs(2),
        )
        .await;
        if data_response.starts_with("354") {
            assert!(result.is_ok());
        } else {
            let failure = result.unwrap_err();
            assert_eq!(
                failure.downcast_ref::<SendFailure>().unwrap().kind,
                SendFailureKind::NotSubmitted
            );
        }
        server.await.unwrap();
    }
}

#[tokio::test]
async fn smtp_data_requires_exact_354_before_uploading_message_bytes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let acceptor = test_acceptor();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = acceptor.accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        greet_and_auth(&mut connection, &["8BITMIME"]).await;
        assert_eq!(
            read_line(&mut connection).await,
            "MAIL FROM:<sender@example.test>\r\n"
        );
        reply(connection.get_mut(), "250 sender accepted\r\n").await;
        assert_eq!(
            read_line(&mut connection).await,
            "RCPT TO:<recipient@example.test>\r\n"
        );
        reply(connection.get_mut(), "250 recipient accepted\r\n").await;
        assert_eq!(read_line(&mut connection).await, "DATA\r\n");
        assert!(connection.buffer().is_empty());
        reply(
            connection.get_mut(),
            "250 wrong-stage completion instead of DATA permission\r\n",
        )
        .await;

        let mut forbidden = String::new();
        let read = timeout(
            Duration::from_millis(300),
            connection.read_line(&mut forbidden),
        )
        .await;
        assert!(
            forbidden.is_empty(),
            "message bytes followed a non-354 DATA reply: {forbidden:?}; read result: {read:?}"
        );
    });

    let mut account = test_account();
    account.smtp_port = port;
    let envelope = Envelope::new(
        Some("sender@example.test".parse().unwrap()),
        vec!["recipient@example.test".parse().unwrap()],
    )
    .unwrap();
    let error = send_smtp_raw(
        &account,
        &envelope,
        b"From: sender@example.test\r\n\r\nforbidden DATA upload fixture\r\n",
        "secret",
        smtp_endpoint(port),
        Duration::from_secs(2),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<SendFailure>().unwrap().kind,
        SendFailureKind::NotSubmitted
    );
    server.await.unwrap();
}

#[tokio::test]
async fn smtp_mail_and_rcpt_intermediate_replies_do_not_advance_the_transaction() {
    for rejected_stage in ["MAIL", "RCPT"] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let acceptor = test_acceptor();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let stream = acceptor.accept(stream).await.unwrap();
            let mut connection = BufReader::new(stream);
            greet_and_auth(&mut connection, &["8BITMIME"]).await;
            assert_eq!(
                read_line(&mut connection).await,
                "MAIL FROM:<sender@example.test>\r\n"
            );
            if rejected_stage == "MAIL" {
                reply(
                    connection.get_mut(),
                    "354 wrong-stage intermediate MAIL reply\r\n",
                )
                .await;
            } else {
                reply(connection.get_mut(), "250 sender accepted\r\n").await;
                assert_eq!(
                    read_line(&mut connection).await,
                    "RCPT TO:<recipient@example.test>\r\n"
                );
                reply(
                    connection.get_mut(),
                    "354 wrong-stage intermediate RCPT reply\r\n",
                )
                .await;
            }

            let mut forbidden = String::new();
            let read = timeout(
                Duration::from_millis(300),
                connection.read_line(&mut forbidden),
            )
            .await;
            assert!(
                forbidden.is_empty(),
                "SMTP advanced after an intermediate {rejected_stage} reply: {forbidden:?}; read result: {read:?}"
            );
        });

        let mut account = test_account();
        account.smtp_port = port;
        let envelope = Envelope::new(
            Some("sender@example.test".parse().unwrap()),
            vec!["recipient@example.test".parse().unwrap()],
        )
        .unwrap();
        let error = send_smtp_raw(
            &account,
            &envelope,
            b"From: sender@example.test\r\n\r\nwrong-stage transaction fixture\r\n",
            "secret",
            smtp_endpoint(port),
            Duration::from_secs(2),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<SendFailure>().unwrap().kind,
            SendFailureKind::NotSubmitted,
            "{rejected_stage} intermediate reply"
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn smtp_intermediate_ehlo_reply_does_not_reach_authentication() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let acceptor = test_acceptor();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = acceptor.accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        reply(connection.get_mut(), "220 localhost standards fixture\r\n").await;
        assert!(read_line(&mut connection).await.starts_with("EHLO "));
        reply(
            connection.get_mut(),
            "354 wrong-stage intermediate EHLO reply\r\n",
        )
        .await;

        let mut forbidden = String::new();
        let read = timeout(
            Duration::from_millis(300),
            connection.read_line(&mut forbidden),
        )
        .await;
        assert!(
            forbidden.is_empty(),
            "SMTP sent another command after an intermediate EHLO reply: {forbidden:?}; read result: {read:?}"
        );
    });

    let mut account = test_account();
    account.smtp_port = port;
    let envelope = Envelope::new(
        Some("sender@example.test".parse().unwrap()),
        vec!["recipient@example.test".parse().unwrap()],
    )
    .unwrap();
    let error = send_smtp_raw(
        &account,
        &envelope,
        b"From: sender@example.test\r\n\r\nwrong-stage EHLO fixture\r\n",
        "secret",
        smtp_endpoint(port),
        Duration::from_secs(2),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<SendFailure>().unwrap().kind,
        SendFailureKind::NotSubmitted
    );
    server.await.unwrap();
}

#[tokio::test]
async fn smtp_command_timeouts_are_scoped_per_protocol_stage() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let acceptor = test_acceptor();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = acceptor.accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        greet_and_auth(&mut connection, &["8BITMIME"]).await;

        assert_eq!(
            read_line(&mut connection).await,
            "MAIL FROM:<sender@example.test>\r\n"
        );
        tokio::time::sleep(Duration::from_millis(600)).await;
        reply(connection.get_mut(), "250 sender accepted\r\n").await;
        assert_eq!(
            read_line(&mut connection).await,
            "RCPT TO:<recipient@example.test>\r\n"
        );
        tokio::time::sleep(Duration::from_millis(600)).await;
        reply(connection.get_mut(), "250 recipient accepted\r\n").await;
        assert_eq!(read_line(&mut connection).await, "DATA\r\n");
        reply(connection.get_mut(), "354 continue\r\n").await;
        loop {
            if read_line(&mut connection).await == ".\r\n" {
                break;
            }
        }
        reply(connection.get_mut(), "250 queued\r\n").await;
    });

    let mut account = test_account();
    account.smtp_port = port;
    let envelope = Envelope::new(
        Some("sender@example.test".parse().unwrap()),
        vec!["recipient@example.test".parse().unwrap()],
    )
    .unwrap();
    let response = send_smtp_raw(
        &account,
        &envelope,
        b"From: sender@example.test\r\n\r\nper-stage timeout fixture\r\n",
        "secret",
        smtp_endpoint(port),
        Duration::from_secs(1),
    )
    .await
    .unwrap();
    assert_eq!(response, "queued");
    server.await.unwrap();
}

#[tokio::test]
async fn smtp_each_recipient_receives_a_fresh_command_budget() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let acceptor = test_acceptor();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = acceptor.accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        greet_and_auth(&mut connection, &["8BITMIME"]).await;
        assert_eq!(
            read_line(&mut connection).await,
            "MAIL FROM:<sender@example.test>\r\n"
        );
        reply(connection.get_mut(), "250 sender accepted\r\n").await;

        for index in 0..7 {
            assert_eq!(
                read_line(&mut connection).await,
                format!("RCPT TO:<recipient-{index}@example.test>\r\n")
            );
            tokio::time::sleep(Duration::from_millis(600)).await;
            reply(connection.get_mut(), "250 recipient accepted\r\n").await;
        }
        assert_eq!(read_line(&mut connection).await, "DATA\r\n");
        reply(connection.get_mut(), "354 continue\r\n").await;
        loop {
            if read_line(&mut connection).await == ".\r\n" {
                break;
            }
        }
        reply(connection.get_mut(), "250 queued\r\n").await;
    });

    let mut account = test_account();
    account.smtp_port = port;
    let envelope = Envelope::new(
        Some("sender@example.test".parse().unwrap()),
        (0..7)
            .map(|index| format!("recipient-{index}@example.test").parse().unwrap())
            .collect(),
    )
    .unwrap();
    let response = send_smtp_raw(
        &account,
        &envelope,
        b"From: sender@example.test\r\n\r\noverall timeout fixture\r\n",
        "secret",
        smtp_endpoint(port),
        Duration::from_secs(1),
    )
    .await
    .unwrap();
    assert_eq!(response, "queued");
    server.await.unwrap();
}

#[tokio::test]
async fn cancelling_a_send_drops_its_exclusive_connection_without_quit_or_reuse() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let acceptor = test_acceptor();
    let (data_seen_tx, data_seen_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = acceptor.accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        greet_and_auth(&mut connection, &["8BITMIME"]).await;
        assert_eq!(
            read_line(&mut connection).await,
            "MAIL FROM:<sender@example.test>\r\n"
        );
        reply(connection.get_mut(), "250 sender accepted\r\n").await;
        assert_eq!(
            read_line(&mut connection).await,
            "RCPT TO:<recipient@example.test>\r\n"
        );
        reply(connection.get_mut(), "250 recipient accepted\r\n").await;
        assert_eq!(read_line(&mut connection).await, "DATA\r\n");
        data_seen_tx.send(()).unwrap();

        let mut forbidden = Vec::new();
        let read = connection.read_to_end(&mut forbidden).await;
        assert!(
            read.is_ok()
                || read
                    .as_ref()
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::UnexpectedEof),
            "cancelled TLS connection ended unexpectedly: {read:?}"
        );
        assert!(
            forbidden.is_empty(),
            "cancelled send wrote DATA content, terminator, or QUIT: {:?}",
            String::from_utf8_lossy(&forbidden)
        );
        assert!(
            timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_err(),
            "cancelled send opened a replacement SMTP connection"
        );
    });

    let mut account = test_account();
    account.smtp_port = port;
    let envelope = Envelope::new(
        Some("sender@example.test".parse().unwrap()),
        vec!["recipient@example.test".parse().unwrap()],
    )
    .unwrap();
    let send = tokio::spawn(async move {
        send_smtp_raw(
            &account,
            &envelope,
            b"From: sender@example.test\r\n\r\ncancelled send fixture\r\n",
            "secret",
            smtp_endpoint(port),
            Duration::from_secs(30),
        )
        .await
    });
    data_seen_rx.await.unwrap();
    send.abort();
    assert!(send.await.unwrap_err().is_cancelled());
    server.await.unwrap();
}

#[tokio::test]
async fn smtp_final_ack_timeout_remains_delivery_uncertain() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let acceptor = test_acceptor();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = acceptor.accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        greet_and_auth(&mut connection, &["8BITMIME"]).await;
        assert_eq!(
            read_line(&mut connection).await,
            "MAIL FROM:<sender@example.test>\r\n"
        );
        reply(connection.get_mut(), "250 sender accepted\r\n").await;
        assert_eq!(
            read_line(&mut connection).await,
            "RCPT TO:<recipient@example.test>\r\n"
        );
        reply(connection.get_mut(), "250 recipient accepted\r\n").await;
        assert_eq!(read_line(&mut connection).await, "DATA\r\n");
        reply(connection.get_mut(), "354 continue\r\n").await;
        loop {
            if read_line(&mut connection).await == ".\r\n" {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    });

    let mut account = test_account();
    account.smtp_port = port;
    let envelope = Envelope::new(
        Some("sender@example.test".parse().unwrap()),
        vec!["recipient@example.test".parse().unwrap()],
    )
    .unwrap();
    let error = send_smtp_raw(
        &account,
        &envelope,
        b"From: sender@example.test\r\n\r\nfinal acknowledgment fixture\r\n",
        "secret",
        smtp_endpoint(port),
        Duration::from_millis(200),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<SendFailure>().unwrap().kind,
        SendFailureKind::DeliveryUncertain
    );
    server.await.unwrap();
}

#[tokio::test]
async fn smtp_intermediate_reply_after_data_terminator_is_uncertain_without_retry_or_sent_append() {
    let smtp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let smtp_port = smtp_listener.local_addr().unwrap().port();
    let imap_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let imap_port = imap_listener.local_addr().unwrap().port();
    let acceptor = test_acceptor();
    let smtp_server = tokio::spawn(async move {
        let (stream, _) = smtp_listener.accept().await.unwrap();
        let stream = acceptor.accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        greet_and_auth(&mut connection, &["8BITMIME"]).await;
        assert_eq!(
            read_line(&mut connection).await,
            "MAIL FROM:<sender@example.test>\r\n"
        );
        reply(connection.get_mut(), "250 sender accepted\r\n").await;
        assert_eq!(
            read_line(&mut connection).await,
            "RCPT TO:<recipient@example.test>\r\n"
        );
        reply(connection.get_mut(), "250 recipient accepted\r\n").await;
        assert_eq!(read_line(&mut connection).await, "DATA\r\n");
        reply(connection.get_mut(), "354 continue\r\n").await;

        loop {
            if read_line(&mut connection).await == ".\r\n" {
                break;
            }
        }
        reply(
            connection.get_mut(),
            "354 unexpected intermediate reply after terminator\r\n",
        )
        .await;

        let retried = timeout(Duration::from_millis(300), smtp_listener.accept())
            .await
            .is_ok();
        (true, retried)
    });
    let imap_observer = tokio::spawn(async move {
        timeout(Duration::from_millis(500), imap_listener.accept())
            .await
            .is_ok()
    });

    let mut account = test_account();
    account.smtp_port = smtp_port;
    account.imap_port = imap_port;
    let service = MailService::new(Store::in_memory().await.unwrap());
    let error = service
        .send_with_smtp_endpoint(
            &account,
            &basic_draft(&account),
            "secret",
            smtp_endpoint(smtp_port),
            Duration::from_secs(2),
        )
        .await
        .unwrap_err();

    assert_eq!(
        error.downcast_ref::<SendFailure>().unwrap().kind,
        SendFailureKind::DeliveryUncertain
    );
    let (received_terminator, retried) = smtp_server.await.unwrap();
    assert!(
        received_terminator,
        "SMTP server did not receive the DATA terminator"
    );
    assert!(
        !retried,
        "uncertain delivery triggered an automatic SMTP retry"
    );
    assert!(
        !imap_observer.await.unwrap(),
        "uncertain delivery attempted to append a copy to Sent"
    );
}

#[tokio::test]
async fn smtp_reply_input_is_bounded_while_the_supported_boundary_remains_usable() {
    for (reply_octets, expect_ehlo) in [(1000usize, true), (1001usize, false)] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let acceptor = test_acceptor();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let stream = acceptor.accept(stream).await.unwrap();
            let mut connection = BufReader::new(stream);
            let greeting = format!("220 {}\r\n", "x".repeat(reply_octets - 6));
            assert_eq!(greeting.len(), reply_octets);
            reply(connection.get_mut(), &greeting).await;

            let mut command = String::new();
            let read = timeout(
                Duration::from_millis(300),
                connection.read_line(&mut command),
            )
            .await;
            if expect_ehlo {
                assert!(command.starts_with("EHLO "), "{command:?}; {read:?}");
                reply(connection.get_mut(), "250-localhost\r\n250 AUTH PLAIN\r\n").await;
                assert!(read_line(&mut connection).await.starts_with("EHLO "));
                reply(connection.get_mut(), "250-localhost\r\n250 AUTH PLAIN\r\n").await;
                assert!(read_line(&mut connection).await.starts_with("AUTH PLAIN "));
                reply(connection.get_mut(), "535 rejected fixture login\r\n").await;
            } else {
                assert!(
                    command.is_empty(),
                    "over-limit SMTP reply advanced the protocol: {command:?}; {read:?}"
                );
            }
        });

        let mut account = test_account();
        account.smtp_port = port;
        let envelope = Envelope::new(
            Some("sender@example.test".parse().unwrap()),
            vec!["recipient@example.test".parse().unwrap()],
        )
        .unwrap();
        let error = send_smtp_raw(
            &account,
            &envelope,
            b"From: sender@example.test\r\n\r\nreply limit fixture\r\n",
            "secret",
            smtp_endpoint(port),
            Duration::from_secs(2),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<SendFailure>().unwrap().kind,
            SendFailureKind::NotSubmitted
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn smtp_utf8_recipient_rejection_is_bounded_and_stops_before_data() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let acceptor = test_acceptor();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = acceptor.accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        greet_and_auth(&mut connection, &["SMTPUTF8", "8BITMIME"]).await;
        let mail = read_line(&mut connection).await;
        assert!(
            mail.starts_with("MAIL FROM:<sender@example.test>"),
            "{mail:?}"
        );
        assert!(mail.contains("SMTPUTF8"), "{mail:?}");
        reply(connection.get_mut(), "250 sender accepted\r\n").await;
        assert_eq!(
            read_line(&mut connection).await,
            "RCPT TO:<r\u{00e9}cipient@example.test>\r\n"
        );
        reply(
            connection.get_mut(),
            "553 5.1.3 adresse refus\u{00e9}e pour ce destinataire \u{1f4e8}\r\n",
        )
        .await;

        let mut forbidden = String::new();
        let read = timeout(
            Duration::from_millis(200),
            connection.read_line(&mut forbidden),
        )
        .await;
        assert!(
            forbidden.is_empty(),
            "UTF-8 recipient rejection was followed by DATA: {forbidden:?}; {read:?}"
        );
    });

    let mut account = test_account();
    account.smtp_port = port;
    let envelope = Envelope::new(
        Some("sender@example.test".parse().unwrap()),
        vec!["r\u{00e9}cipient@example.test".parse().unwrap()],
    )
    .unwrap();
    let error = send_smtp_raw(
        &account,
        &envelope,
        b"From: sender@example.test\r\n\r\nUTF-8 rejection fixture\r\n",
        "secret",
        smtp_endpoint(port),
        Duration::from_secs(2),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<SendFailure>().unwrap().kind,
        SendFailureKind::NotSubmitted
    );
    server.await.unwrap();
}

#[tokio::test]
async fn smtp_utf8_forwarding_reply_forms_preserve_submission_state() {
    for reply_code in [251u16, 551u16] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let acceptor = test_acceptor();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let stream = acceptor.accept(stream).await.unwrap();
            let mut connection = BufReader::new(stream);
            greet_and_auth(&mut connection, &["SMTPUTF8", "8BITMIME"]).await;
            let mail = read_line(&mut connection).await;
            assert!(mail.contains("SMTPUTF8"), "{mail:?}");
            reply(connection.get_mut(), "250 sender accepted\r\n").await;
            assert_eq!(
                read_line(&mut connection).await,
                "RCPT TO:<r\u{00e9}cipient@example.test>\r\n"
            );
            reply(
                connection.get_mut(),
                &format!(
                    "{reply_code} {} user moved to <autr\u{00e9}@exemple.test>\r\n",
                    if reply_code == 251 { "2.1.5" } else { "5.1.6" }
                ),
            )
            .await;

            if reply_code == 251 {
                assert_eq!(read_line(&mut connection).await, "DATA\r\n");
                reply(connection.get_mut(), "354 continue\r\n").await;
                loop {
                    if read_line(&mut connection).await == ".\r\n" {
                        break;
                    }
                }
                reply(connection.get_mut(), "250 queued\r\n").await;
            } else {
                let mut forbidden = String::new();
                let read = timeout(
                    Duration::from_millis(200),
                    connection.read_line(&mut forbidden),
                )
                .await;
                assert!(
                    forbidden.is_empty(),
                    "551 rejection was followed by DATA: {forbidden:?}; {read:?}"
                );
            }
        });

        let mut account = test_account();
        account.smtp_port = port;
        let envelope = Envelope::new(
            Some("sender@example.test".parse().unwrap()),
            vec!["r\u{00e9}cipient@example.test".parse().unwrap()],
        )
        .unwrap();
        let result = send_smtp_raw(
            &account,
            &envelope,
            b"From: sender@example.test\r\n\r\nUTF-8 forwarding reply fixture\r\n",
            "secret",
            smtp_endpoint(port),
            Duration::from_secs(2),
        )
        .await;
        if reply_code == 251 {
            assert!(result.is_ok());
        } else {
            assert_eq!(
                result
                    .unwrap_err()
                    .downcast_ref::<SendFailure>()
                    .unwrap()
                    .kind,
                SendFailureKind::NotSubmitted
            );
        }
        server.await.unwrap();
    }
}

#[tokio::test]
async fn smtp_declares_utf8_and_eight_bit_body_and_dot_stuffs_the_wire_message() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let acceptor = test_acceptor();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = acceptor.accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        greet_and_auth(&mut connection, &["SMTPUTF8", "8BITMIME"]).await;

        let mail = read_line(&mut connection).await;
        assert!(
            mail.starts_with("MAIL FROM:<s\u{00f8}ren@example.test>"),
            "{mail:?}"
        );
        assert!(mail.contains("SMTPUTF8"), "{mail:?}");
        assert!(mail.contains("BODY=8BITMIME"), "{mail:?}");
        reply(
            connection.get_mut(),
            "250-first line\r\n250 sender accepted\r\n",
        )
        .await;
        assert_eq!(
            read_line(&mut connection).await,
            "RCPT TO:<recipient@example.test>\r\n"
        );
        reply(connection.get_mut(), "250 recipient accepted\r\n").await;
        assert_eq!(read_line(&mut connection).await, "DATA\r\n");
        reply(connection.get_mut(), "354 continue\r\n").await;

        let mut wire_lines = Vec::new();
        loop {
            let line = read_line(&mut connection).await;
            if line == ".\r\n" {
                break;
            }
            wire_lines.push(line);
        }
        assert!(wire_lines.iter().any(|line| line == "..leading dot\r\n"));
        assert!(wire_lines.iter().any(|line| line == "...two dots\r\n"));
        reply(
            connection.get_mut(),
            "250-first acceptance detail\r\n250 queued fixture-utf8\r\n",
        )
        .await;
    });

    let mut account = test_account();
    account.smtp_port = port;
    let envelope = Envelope::new(
        Some("s\u{00f8}ren@example.test".parse().unwrap()),
        vec!["recipient@example.test".parse().unwrap()],
    )
    .unwrap();
    let response = send_smtp_raw(
        &account,
        &envelope,
        b"From: s\xc3\xb8ren@example.test\r\nTo: recipient@example.test\r\n\r\nUTF-8: \xc3\xa9\r\n.leading dot\r\n..two dots\r\n",
        "secret",
        smtp_endpoint(port),
        Duration::from_secs(2),
    )
    .await
    .unwrap();
    assert!(response.contains("first acceptance detail"));
    assert!(response.contains("queued fixture-utf8"));
    server.await.unwrap();
}

#[tokio::test]
async fn bcc_is_only_an_envelope_recipient_in_the_transmitted_message() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let acceptor = test_acceptor();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = acceptor.accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        greet_and_auth(&mut connection, &["8BITMIME"]).await;
        assert!(read_line(&mut connection)
            .await
            .starts_with("MAIL FROM:<sender@example.test>"));
        reply(connection.get_mut(), "250 sender accepted\r\n").await;
        let mut recipients = Vec::new();
        for _ in 0..3 {
            recipients.push(read_line(&mut connection).await);
            reply(connection.get_mut(), "250 recipient accepted\r\n").await;
        }
        assert_eq!(
            recipients,
            vec![
                "RCPT TO:<visible@example.test>\r\n",
                "RCPT TO:<copy@example.test>\r\n",
                "RCPT TO:<hidden@example.test>\r\n",
            ]
        );
        assert_eq!(read_line(&mut connection).await, "DATA\r\n");
        reply(connection.get_mut(), "354 continue\r\n").await;
        let mut wire = String::new();
        loop {
            let line = read_line(&mut connection).await;
            if line == ".\r\n" {
                break;
            }
            wire.push_str(&line);
        }
        assert!(wire.contains("To: visible@example.test"));
        assert!(wire.contains("Cc: copy@example.test"));
        assert!(!wire.contains("Bcc:"), "hidden recipient leaked: {wire}");
        assert!(
            !wire.contains("hidden@example.test"),
            "hidden recipient leaked: {wire}"
        );
        reply(connection.get_mut(), "250 queued\r\n").await;
    });

    let mut account = test_account();
    account.smtp_port = port;
    let draft = ComposeMessage {
        account_id: account.id,
        to: vec!["visible@example.test".into()],
        cc: vec!["copy@example.test".into()],
        bcc: vec!["hidden@example.test".into()],
        subject: "Envelope privacy".into(),
        body_text: "The body has no hidden recipient.".into(),
        body_html: None,
        in_reply_to: None,
        references: None,
        attachments: Vec::new(),
    };
    let message = build_compose_message(&account, &draft).unwrap();
    let formatted = message.formatted();
    send_smtp_raw(
        &account,
        message.envelope(),
        &formatted,
        "secret",
        smtp_endpoint(port),
        Duration::from_secs(2),
    )
    .await
    .unwrap();
    server.await.unwrap();
}

fn basic_draft(account: &Account) -> ComposeMessage {
    ComposeMessage {
        account_id: account.id,
        to: vec!["recipient@example.test".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "Safe subject".into(),
        body_text: "Body".into(),
        body_html: None,
        in_reply_to: None,
        references: None,
        attachments: Vec::new(),
    }
}

fn unfolded_fields(wire: &str, field_name: &str) -> Vec<String> {
    let prefix = format!("{field_name}:");
    let mut fields = Vec::new();
    let mut current: Option<String> = None;
    for line in wire.split("\r\n") {
        if line.starts_with(&prefix) {
            if let Some(field) = current.take() {
                fields.push(field);
            }
            current = Some(line.to_owned());
        } else if line.starts_with([' ', '\t']) {
            if let Some(field) = current.as_mut() {
                field.push(' ');
                field.push_str(line.trim_start());
            }
        } else if let Some(field) = current.take() {
            fields.push(field);
        }
    }
    if let Some(field) = current {
        fields.push(field);
    }
    fields
}

fn generated_boundaries(wire: &str) -> Vec<String> {
    unfolded_fields(wire, "Content-Type")
        .into_iter()
        .filter_map(|field| {
            let value = field.split_once("boundary=\"")?.1;
            Some(value.split_once('"')?.0.to_owned())
        })
        .collect()
}

fn generated_header_lines<'a>(wire: &'a str, boundaries: &[String]) -> Vec<&'a str> {
    let mut lines = Vec::new();
    let mut in_headers = true;
    for line in wire.split("\r\n") {
        if in_headers {
            if line.is_empty() {
                in_headers = false;
            } else {
                lines.push(line);
            }
        } else if boundaries
            .iter()
            .any(|boundary| line == format!("--{boundary}"))
        {
            in_headers = true;
        }
    }
    lines
}

#[tokio::test]
async fn production_send_serializes_nested_boundaries_and_unicode_filename_safely() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let acceptor = test_acceptor();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = acceptor.accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        greet_and_auth(&mut connection, &["8BITMIME"]).await;

        assert_eq!(
            read_line(&mut connection).await,
            "MAIL FROM:<sender@example.test>\r\n"
        );
        reply(connection.get_mut(), "250 sender accepted\r\n").await;
        for recipient in [
            "visible@example.test",
            "copy@example.test",
            "hidden@example.test",
        ] {
            assert_eq!(
                read_line(&mut connection).await,
                format!("RCPT TO:<{recipient}>\r\n")
            );
            reply(connection.get_mut(), "250 recipient accepted\r\n").await;
        }
        assert_eq!(read_line(&mut connection).await, "DATA\r\n");
        reply(connection.get_mut(), "354 continue\r\n").await;

        let mut wire = String::new();
        loop {
            let line = read_line(&mut connection).await;
            if line == ".\r\n" {
                break;
            }
            wire.push_str(&line);
        }
        reply(connection.get_mut(), "250 queued nested-fixture\r\n").await;
        wire
    });

    let mut account = test_account();
    account.smtp_port = port;
    account.smtp_host = "smtp.gmail.com".into();
    account.display_name = "Fictional Séndér with a deliberately long display name".into();
    let filename = "résumé 財務 quarterly results 2026 final signed copy №42.pdf";
    let draft = ComposeMessage {
        account_id: account.id,
        to: vec!["Visible Réader with a long display name <visible@example.test>".into()],
        cc: vec!["Cöpy Réader with a long display name <copy@example.test>".into()],
        bcc: vec!["Hidden Réader <hidden@example.test>".into()],
        subject: "International quarterly report ".to_owned() + &"🌍".repeat(48),
        body_text: "Plain fallback for the nested production fixture.".into(),
        body_html: Some(
            "<p>HTML body with an embedded image.</p><img src=\"data:image/png;base64,aW1hZ2U=\">"
                .into(),
        ),
        in_reply_to: None,
        references: None,
        attachments: vec![ComposeAttachment {
            filename: filename.into(),
            mime_type: "application/pdf".into(),
            content_base64: STANDARD.encode(b"fictional attachment bytes"),
        }],
    };
    let service = MailService::new(Store::in_memory().await.unwrap());
    let response = service
        .send_with_smtp_endpoint(
            &account,
            &draft,
            "secret",
            smtp_endpoint(port),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
    assert!(response.contains("queued nested-fixture"));
    let wire = server.await.unwrap();

    let bytes = wire.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            assert_eq!(bytes.get(index.wrapping_sub(1)), Some(&b'\r'), "bare LF");
        }
        if *byte == b'\r' {
            assert_eq!(bytes.get(index + 1), Some(&b'\n'), "bare CR");
        }
    }

    assert!(!wire.contains("Bcc:"), "Bcc header leaked: {wire}");
    assert!(
        !wire.contains("hidden@example.test"),
        "Bcc address leaked: {wire}"
    );
    assert!(wire.contains("visible@example.test"));
    assert!(wire.contains("copy@example.test"));

    let content_types = unfolded_fields(&wire, "Content-Type");
    let boundaries = generated_boundaries(&wire);
    assert_eq!(
        boundaries.len(),
        3,
        "expected mixed, alternative, and related boundaries: {content_types:?}"
    );
    let mut unique_boundaries = boundaries.clone();
    unique_boundaries.sort();
    unique_boundaries.dedup();
    assert_eq!(unique_boundaries.len(), boundaries.len());
    for boundary in &boundaries {
        assert!(!boundary.is_empty());
        assert!(boundary.is_ascii());
        assert!(
            boundary.len() <= 70,
            "boundary exceeds RFC 2046 limit: {boundary:?}"
        );
        assert!(
            content_types
                .iter()
                .any(|field| field.contains(&format!("boundary=\"{boundary}\""))),
            "generated boundary parameter was not quoted: {boundary:?}"
        );
        assert!(boundary.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'\''
                        | b'('
                        | b')'
                        | b'+'
                        | b'_'
                        | b','
                        | b'-'
                        | b'.'
                        | b'/'
                        | b':'
                        | b'='
                        | b'?'
                        | b' '
                )
        }));
        assert_ne!(boundary.as_bytes().last(), Some(&b' '));

        let opening = format!("--{boundary}");
        let closing = format!("--{boundary}--");
        let delimiter_lines: Vec<_> = wire
            .split("\r\n")
            .filter(|line| *line == opening || *line == closing)
            .collect();
        assert!(delimiter_lines.iter().any(|line| **line == opening));
        assert_eq!(
            delimiter_lines
                .iter()
                .filter(|line| ***line == closing)
                .count(),
            1,
            "boundary must have exactly one matching closing delimiter: {boundary:?}"
        );
    }

    let header_lines = generated_header_lines(&wire, &boundaries);
    assert!(
        header_lines.iter().all(|line| line.len() <= 998),
        "generated header line exceeded the RFC 5322 hard limit: {:?}",
        header_lines
            .iter()
            .filter(|line| line.len() > 998)
            .collect::<Vec<_>>()
    );
    let over_recommended_length: Vec<_> = header_lines
        .iter()
        .copied()
        .filter(|line| line.len() > 78)
        .collect();
    // RFC 5322 section 2.1.1 says implementations SHOULD keep lines to 78
    // characters, while the 998-character limit above is mandatory. Lettre
    // currently keeps these two encoded display-name fields on one line. Pin
    // that narrow, visible tolerance so later serialization changes cannot
    // expand it silently or be mistaken for complete preferred folding.
    assert_eq!(over_recommended_length.len(), 2);
    assert!(over_recommended_length
        .iter()
        .all(|line| line.starts_with("To:") || line.starts_with("Cc:")));
    assert!(over_recommended_length.iter().all(|line| line.len() <= 110));

    let dispositions = unfolded_fields(&wire, "Content-Disposition");
    assert!(
        dispositions.iter().any(|field| field.contains("filename*")),
        "long Unicode filename was not emitted with RFC 2231 parameters: {dispositions:?}"
    );
    let parsed = parse_complete_message(wire.as_bytes()).unwrap();
    assert_eq!(parsed.subject(), Some(draft.subject.as_str()));
    assert_eq!(
        parsed.body_text(0).as_deref(),
        Some("Plain fallback for the nested production fixture.")
    );
    let parsed_html = parsed.body_html(0).expect("generated HTML body must parse");
    assert!(parsed_html.contains("HTML body with an embedded image."));

    let mixed = parsed.part(0).expect("root mixed part");
    assert_eq!(mime_type(mixed), "multipart/mixed");
    let mixed_children = mixed.sub_parts().expect("mixed children");
    assert_eq!(mixed_children.len(), 2);
    let alternative = parsed.part(mixed_children[0]).expect("alternative part");
    assert_eq!(mime_type(alternative), "multipart/alternative");
    assert_eq!(
        mime_type(parsed.part(mixed_children[1]).expect("PDF attachment part")),
        "application/pdf"
    );
    let alternative_children = alternative.sub_parts().expect("alternative children");
    assert_eq!(alternative_children.len(), 2);
    assert_eq!(
        mime_type(
            parsed
                .part(alternative_children[0])
                .expect("plain text alternative")
        ),
        "text/plain"
    );
    let related = parsed
        .part(alternative_children[1])
        .expect("related HTML part");
    assert_eq!(mime_type(related), "multipart/related");
    let related_children = related.sub_parts().expect("related children");
    assert_eq!(related_children.len(), 2);
    assert_eq!(
        mime_type(parsed.part(related_children[0]).expect("HTML related root")),
        "text/html"
    );
    let inline_image = parsed
        .part(related_children[1])
        .expect("embedded image part");
    assert_eq!(mime_type(inline_image), "image/png");
    let inline_content_id = inline_image
        .content_id()
        .expect("embedded image Content-ID")
        .trim()
        .trim_matches(['<', '>']);
    assert!(parsed_html.contains(&format!("src=\"cid:{inline_content_id}\"")));
    assert_eq!(inline_image.contents(), b"image");

    for (address, expected_name, expected_email) in [
        (
            parsed.from().and_then(|addresses| addresses.first()),
            account.display_name.as_str(),
            account.email.as_str(),
        ),
        (
            parsed.to().and_then(|addresses| addresses.first()),
            "Visible Réader with a long display name",
            "visible@example.test",
        ),
        (
            parsed.cc().and_then(|addresses| addresses.first()),
            "Cöpy Réader with a long display name",
            "copy@example.test",
        ),
    ] {
        let address = address.expect("generated address header must parse");
        assert_eq!(address.name.as_deref(), Some(expected_name));
        assert_eq!(address.address.as_deref(), Some(expected_email));
    }
    assert_eq!(
        parsed
            .from()
            .and_then(|addresses| addresses.as_list())
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        parsed
            .to()
            .and_then(|addresses| addresses.as_list())
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        parsed
            .cc()
            .and_then(|addresses| addresses.as_list())
            .unwrap()
            .len(),
        1
    );
    assert!(parsed.bcc().is_none());
    let attachments = extract_attachments(&parsed, "outbound-standards-fixture").unwrap();
    let attachment = attachments
        .iter()
        .find(|attachment| !attachment.attachment.is_inline)
        .expect("ordinary attachment survives the SMTP wire round trip");
    assert_eq!(attachment.attachment.filename, filename);
    assert_eq!(attachment.bytes, b"fictional attachment bytes");
}

fn ascii_mailbox_with_length(length: usize) -> String {
    mailbox_with_local_and_length(&"l".repeat(64), length)
}

fn mailbox_with_local_and_length(local: &str, length: usize) -> String {
    let domain_length = length - local.len() - 1;
    let mut labels = Vec::new();
    let mut remaining = domain_length;
    while remaining > 63 {
        labels.push("d".repeat(63));
        remaining -= 64;
    }
    labels.push("d".repeat(remaining));
    let mailbox = format!("{local}@{}", labels.join("."));
    assert_eq!(mailbox.len(), length);
    mailbox
}

#[tokio::test]
async fn smtp_exact_maximum_path_stays_within_the_command_line_limit() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let acceptor = test_acceptor();
    let maximum_mailbox = ascii_mailbox_with_length(254);
    let expected_mailbox = maximum_mailbox.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = acceptor.accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        greet_and_auth(&mut connection, &["8BITMIME"]).await;
        let mail = read_line(&mut connection).await;
        assert_eq!(mail, "MAIL FROM:<sender@example.test>\r\n");
        assert!(mail.len() <= 512, "MAIL command exceeded 512 octets");
        reply(connection.get_mut(), "250 sender accepted\r\n").await;
        let rcpt = read_line(&mut connection).await;
        assert_eq!(rcpt, format!("RCPT TO:<{expected_mailbox}>\r\n"));
        assert!(rcpt.len() <= 512, "RCPT command exceeded 512 octets");
        reply(connection.get_mut(), "250 recipient accepted\r\n").await;
        assert_eq!(read_line(&mut connection).await, "DATA\r\n");
        reply(connection.get_mut(), "354 continue\r\n").await;
        loop {
            if read_line(&mut connection).await == ".\r\n" {
                break;
            }
        }
        reply(connection.get_mut(), "250 queued\r\n").await;
    });

    let mut account = test_account();
    account.smtp_port = port;
    let mut draft = basic_draft(&account);
    draft.to = vec![maximum_mailbox];
    let message = build_compose_message(&account, &draft).unwrap();
    send_smtp_raw(
        &account,
        message.envelope(),
        &message.formatted(),
        "secret",
        smtp_endpoint(port),
        Duration::from_secs(2),
    )
    .await
    .unwrap();
    server.await.unwrap();
}

#[test]
fn production_compose_and_mime_parse_removes_the_transport_crlf() {
    let account = test_account();
    let mut draft = basic_draft(&account);
    draft.body_text = "Dakia roundtrip transport marker".into();
    let raw = build_compose_message(&account, &draft).unwrap().formatted();
    let parsed = parse_complete_message(&raw).unwrap();
    let decoded = parsed
        .parts
        .iter()
        .find_map(|part| part.text_contents())
        .expect("outbound plain-text part must parse through Dakia's configured MIME parser");

    assert_eq!(decoded, "Dakia roundtrip transport marker");
}

#[test]
fn generated_messages_have_unique_sender_scoped_message_ids_without_hostname_leakage() {
    let account = test_account();
    let draft = basic_draft(&account);
    let first = String::from_utf8(build_compose_message(&account, &draft).unwrap().formatted())
        .expect("generated MIME is UTF-8");
    let second = String::from_utf8(build_compose_message(&account, &draft).unwrap().formatted())
        .expect("generated MIME is UTF-8");
    let first_headers = first.split_once("\r\n\r\n").unwrap().0;
    let second_headers = second.split_once("\r\n\r\n").unwrap().0;
    let ids = first_headers
        .lines()
        .filter_map(|line| line.strip_prefix("Message-ID: "))
        .collect::<Vec<_>>();
    assert_eq!(
        ids.len(),
        1,
        "exactly one generated Message-ID: {first_headers}"
    );
    let first_id = ids[0];
    assert!(first_id.starts_with('<') && first_id.ends_with("@example.test>"));
    assert!(!first_id.contains("localhost"));
    let second_id = second_headers
        .lines()
        .find_map(|line| line.strip_prefix("Message-ID: "))
        .expect("second generated Message-ID");
    assert_ne!(
        first_id, second_id,
        "each outbound message receives a fresh Message-ID"
    );

    let parsed = parse_complete_message(first.as_bytes()).expect("parse generated message");
    assert_eq!(
        canonical_message_ids_from_message(&parsed, "Message-ID").as_deref(),
        Some(first_id),
        "the configured parser retains the transmitted generated Message-ID"
    );

    let mut internationalized = account.clone();
    internationalized.email = "sender@b\u{fc}cher.example".into();
    let idn = String::from_utf8(
        build_compose_message(&internationalized, &basic_draft(&internationalized))
            .expect("validated internationalized sender")
            .formatted(),
    )
    .expect("generated MIME is UTF-8");
    assert!(
        idn.lines()
            .any(|line| line.starts_with("Message-ID: <")
                && line.ends_with("@xn--bcher-kva.example>")),
        "the generated id-right uses a wire-safe IDNA domain: {idn}"
    );

    // Lettre's SMTP envelope separately rejects address-literal senders, so
    // preserve the narrower Message-ID contract here without expanding the
    // application's existing outbound-address support.
    for literal in ["sender@[192.0.2.1]", "sender@[2001:db8::1]"] {
        let literal = literal
            .parse::<lettre::Address>()
            .expect("letter address parser accepts RFC domain literals");
        let domain = literal.domain();
        assert!(
            generated_message_id(&literal)
                .expect("domain literal Message-ID")
                .ends_with(&format!("@{domain}>")),
            "domain literals remain explicitly scoped and never fall back to a hostname"
        );
    }
}

#[test]
fn public_compose_validation_has_no_transport_side_effect_and_reuses_outbound_rules() {
    let account = test_account();
    let mut invalid = basic_draft(&account);
    invalid.subject = "subject\r\nBcc: fictional@example.test".into();
    assert!(validate_compose_message_for_account(&account, &invalid).is_err());
    assert!(validate_compose_message_for_account(&account, &basic_draft(&account)).is_ok());
}

#[test]
fn generated_mime_has_single_required_fields_and_bounded_encoded_words() {
    let mut account = test_account();
    account.display_name = "Fictional Sender ".to_owned() + &"\u{1f4e8}".repeat(80);
    let mut draft = basic_draft(&account);
    draft.subject = "International subject ".to_owned() + &"\u{1f30d}".repeat(160);

    let formatted =
        String::from_utf8(build_compose_message(&account, &draft).unwrap().formatted()).unwrap();
    let headers = formatted.split_once("\r\n\r\n").unwrap().0;

    for field in ["Date", "From", "MIME-Version"] {
        assert_eq!(
            headers
                .lines()
                .filter(|line| {
                    line.split_once(':')
                        .is_some_and(|(name, _)| name.eq_ignore_ascii_case(field))
                })
                .count(),
            1,
            "generated message must contain exactly one {field} field:\n{headers}"
        );
    }
    assert!(headers.contains("MIME-Version: 1.0"), "{headers}");

    let subject_start = headers.find("Subject:").unwrap();
    let subject_tail = &headers[subject_start..];
    let subject_end = subject_tail
        .find("\r\nContent-Type:")
        .or_else(|| subject_tail.find("\r\nDate:"))
        .unwrap_or(subject_tail.len());
    let subject_field = &subject_tail[..subject_end];
    assert!(
        subject_field.contains("\r\n "),
        "long international Subject was not folded: {subject_field:?}"
    );

    let encoded_words: Vec<_> = headers
        .split_ascii_whitespace()
        .filter(|word| word.starts_with("=?") && word.ends_with("?="))
        .collect();
    assert!(
        !encoded_words.is_empty(),
        "expected encoded words: {headers}"
    );
    assert!(
        encoded_words.iter().all(|word| word.len() <= 75),
        "generated encoded word exceeded 75 characters: {encoded_words:?}"
    );
    assert!(
        headers.lines().all(|line| line.len() <= 998),
        "generated header line exceeded 998 octets: {headers}"
    );
}

#[test]
fn generated_mime_keeps_every_transmitted_text_line_within_1000_octets() {
    let account = test_account();
    for body in ["a".repeat(998), "b".repeat(999), "c".repeat(4096)] {
        let mut draft = basic_draft(&account);
        draft.body_text = body;
        let formatted = build_compose_message(&account, &draft).unwrap().formatted();
        for line in formatted.split(|byte| *byte == b'\n') {
            assert!(
                line.len() <= 999,
                "generated SMTP text line exceeded 1000 octets including LF: {}",
                line.len() + 1
            );
        }
    }
}

#[test]
fn outbound_smtp_paths_observe_the_256_octet_limit() {
    let account = test_account();
    let exact_mailbox = ascii_mailbox_with_length(254);
    let mut exact = basic_draft(&account);
    exact.to = vec![exact_mailbox.clone()];
    let exact_message = build_compose_message(&account, &exact).unwrap();
    assert_eq!(exact_message.envelope().to()[0].to_string(), exact_mailbox);
    assert_eq!(format!("<{}>", exact_message.envelope().to()[0]).len(), 256);

    let mut exact_sender = account.clone();
    exact_sender.email = ascii_mailbox_with_length(254);
    exact_sender.display_name = "Display names are not part of the SMTP path ".repeat(20);
    assert!(build_compose_message(&exact_sender, &basic_draft(&exact_sender)).is_ok());

    let over_mailbox = ascii_mailbox_with_length(255);
    for field in ["To", "Cc", "Bcc"] {
        let mut over = basic_draft(&account);
        match field {
            "To" => over.to = vec![over_mailbox.clone()],
            "Cc" => over.cc = vec![over_mailbox.clone()],
            "Bcc" => over.bcc = vec![over_mailbox.clone()],
            _ => unreachable!(),
        }
        assert!(
            build_compose_message(&account, &over).is_err(),
            "Dakia accepted a {field} forward-path longer than 256 octets"
        );
    }

    let utf8_local = "\u{00e9}".repeat(30);
    let exact_utf8 = mailbox_with_local_and_length(&utf8_local, 254);
    let mut exact_utf8_draft = basic_draft(&account);
    exact_utf8_draft.to = vec![exact_utf8.clone()];
    assert!(build_compose_message(&account, &exact_utf8_draft).is_ok());

    let over_utf8 = mailbox_with_local_and_length(&utf8_local, 255);
    for field in ["To", "Cc", "Bcc"] {
        let mut over = basic_draft(&account);
        match field {
            "To" => over.to = vec![over_utf8.clone()],
            "Cc" => over.cc = vec![over_utf8.clone()],
            "Bcc" => over.bcc = vec![over_utf8.clone()],
            _ => unreachable!(),
        }
        assert!(
            build_compose_message(&account, &over).is_err(),
            "Dakia accepted a UTF-8 {field} forward-path longer than 256 octets"
        );
    }

    let mut exact_utf8_sender = account.clone();
    exact_utf8_sender.email = exact_utf8;
    exact_utf8_sender.display_name = "Fictional display name ".repeat(20);
    assert!(build_compose_message(&exact_utf8_sender, &basic_draft(&exact_utf8_sender)).is_ok());
    let mut over_utf8_sender = account.clone();
    over_utf8_sender.email = over_utf8;
    assert!(build_compose_message(&over_utf8_sender, &basic_draft(&over_utf8_sender)).is_err());

    let mut over_sender = account.clone();
    over_sender.email = over_mailbox;
    assert!(build_compose_message(&over_sender, &basic_draft(&over_sender)).is_err());
}

#[tokio::test]
async fn invalid_smtp_path_fails_before_opening_the_smtp_transport() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut account = test_account();
    account.smtp_port = port;
    let mut draft = basic_draft(&account);
    draft.to = vec![ascii_mailbox_with_length(255)];
    let service = MailService::new(Store::in_memory().await.unwrap());

    let error = service
        .send_with_smtp_endpoint(
            &account,
            &draft,
            "secret",
            smtp_endpoint(port),
            Duration::from_millis(500),
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<SendFailure>().unwrap().kind,
        SendFailureKind::NotSubmitted
    );
    assert!(
        timeout(Duration::from_millis(200), listener.accept())
            .await
            .is_err(),
        "invalid SMTP path opened an SMTP transport connection"
    );
}

#[test]
fn outbound_headers_reject_injection_and_encode_untrusted_filenames() {
    let account = test_account();

    let mut recipient = basic_draft(&account);
    recipient.to = vec!["recipient@example.test\r\nBcc: victim@example.test".into()];
    assert!(build_compose_message(&account, &recipient).is_err());

    let mut subject = basic_draft(&account);
    subject.subject = "Hello\r\nBcc: victim@example.test".into();
    assert!(build_compose_message(&account, &subject).is_err());

    let mut references = basic_draft(&account);
    references.references = Some("<root@example.test>\r\nBcc: victim@example.test".into());
    assert!(build_compose_message(&account, &references).is_err());

    let mut reply = basic_draft(&account);
    reply.in_reply_to = Some("<parent@example.test>\r\nBcc: victim@example.test".into());
    assert!(build_compose_message(&account, &reply).is_err());

    let mut filename = basic_draft(&account);
    filename.attachments.push(ComposeAttachment {
        filename: "report.pdf\r\nBcc: victim@example.test".into(),
        mime_type: "application/pdf".into(),
        content_base64: STANDARD.encode(b"fixture"),
    });
    let formatted = String::from_utf8(
        build_compose_message(&account, &filename)
            .unwrap()
            .formatted(),
    )
    .unwrap();
    assert!(!formatted.contains("\r\nBcc: victim@example.test"));
    assert_eq!(formatted.matches("\r\nBcc:").count(), 0);
}

#[test]
fn catalogue_dates_use_provider_evidence_and_never_the_local_clock() {
    let account = test_account();
    let cases = [
        (
            "missing Date with INTERNALDATE",
            b"From: Sender <sender@example.test>\r\nSubject: Missing\r\n\r\n".as_slice(),
            vec!["* 1 FETCH (UID 7 INTERNALDATE \"09-Feb-2026 07:16:47 +0100\")\r\n".into()],
            Some("2026-02-09T06:16:47+00:00"),
        ),
        (
            "invalid Date with INTERNALDATE",
            b"Date: not-a-date\r\nFrom: Sender <sender@example.test>\r\nSubject: Invalid\r\n\r\n"
                .as_slice(),
            vec!["* 1 FETCH (UID 7 INTERNALDATE \"09-Feb-2026 07:16:47 +0100\")\r\n".into()],
            Some("2026-02-09T06:16:47+00:00"),
        ),
        (
            "missing all provider dates",
            b"From: Sender <sender@example.test>\r\nSubject: Missing\r\n\r\n".as_slice(),
            vec!["* 1 FETCH (UID 7 FLAGS ())\r\n".into()],
            None,
        ),
    ];

    for (name, headers, response, expected) in cases {
        let result = parse_catalog_message(&account, "INBOX", 7, &response, headers, String::new());
        match expected {
            Some(timestamp) => assert_eq!(
                result.unwrap().received_at.to_rfc3339(),
                timestamp,
                "{name}"
            ),
            None => assert!(result.is_err(), "{name} invented a local timestamp"),
        }
    }
}

#[tokio::test]
async fn imap_tls_rejects_a_trusted_certificate_for_the_wrong_hostname() {
    #[derive(Debug)]
    enum ServerObservation {
        PeerRejected,
        ApplicationData(String),
        ClosedWithoutAlert,
        SilentAfterGreeting,
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let acceptor = test_acceptor();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let Ok(mut stream) = acceptor.accept(stream).await else {
            return ServerObservation::PeerRejected;
        };
        if stream
            .write_all(b"* OK synthetic IMAP server ready\r\n")
            .await
            .is_err()
            || stream.flush().await.is_err()
        {
            return ServerObservation::PeerRejected;
        }
        let mut connection = BufReader::new(stream);
        let mut command = String::new();
        match timeout(
            Duration::from_millis(500),
            connection.read_line(&mut command),
        )
        .await
        {
            Ok(Err(_)) => ServerObservation::PeerRejected,
            Ok(Ok(0)) => ServerObservation::ClosedWithoutAlert,
            Ok(Ok(_)) => ServerObservation::ApplicationData(command),
            Err(_) => ServerObservation::SilentAfterGreeting,
        }
    });
    let mut account = test_account();
    account.imap_port = port;
    let endpoint = MailService::imap_loopback_endpoint(
        "127.0.0.1".into(),
        port,
        "mail.invalid".into(),
        STANDARD.decode(TEST_CA_DER_BASE64).unwrap(),
    )
    .unwrap();
    let result = ImapClient::connect_with_endpoint(&account, endpoint).await;
    let error = match result {
        Ok(_) => panic!("hostname-mismatched certificate was accepted"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("TLS handshake failed"),
        "{error:#}"
    );
    let observation = server.await.unwrap();
    if let ServerObservation::ApplicationData(command) = &observation {
        panic!("IMAP sent application data after accepting a hostname mismatch: {command:?}");
    }
    assert!(
        matches!(&observation, ServerObservation::PeerRejected),
        "hostname mismatch was not rejected at TLS: {observation:?}"
    );
}
