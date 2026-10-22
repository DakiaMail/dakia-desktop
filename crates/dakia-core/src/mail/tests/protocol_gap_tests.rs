//! Deterministic boundary regressions for connection, IMAP, and SMTP gaps.

use super::*;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    time::{sleep, timeout, Duration},
};

const SHORT_DEADLINE: Duration = Duration::from_millis(75);

fn protocol_test_draft(account: &Account) -> ComposeMessage {
    ComposeMessage {
        account_id: account.id,
        to: vec!["recipient@example.test".into()],
        cc: vec![],
        bcc: vec![],
        subject: "Safe subject".into(),
        body_text: "Safe body".into(),
        body_html: None,
        in_reply_to: None,
        references: None,
        attachments: vec![],
    }
}

#[test]
fn outbound_header_injection_matrix_never_creates_an_attacker_header() {
    let account = test_account();
    let injected = "value\r\nX-Dakia-Injection: yes";

    for recipient_field in ["to", "cc", "bcc"] {
        let mut draft = protocol_test_draft(&account);
        let value =
            format!("Recipient <recipient@example.test>\r\nX-Dakia-Injection: {recipient_field}");
        match recipient_field {
            "to" => draft.to = vec![value],
            "cc" => draft.cc = vec![value],
            "bcc" => draft.bcc = vec![value],
            _ => unreachable!(),
        }
        assert!(
            build_compose_message(&account, &draft).is_err(),
            "{recipient_field} accepted an injected address"
        );
    }

    for header_field in ["subject", "in-reply-to", "references"] {
        let mut draft = protocol_test_draft(&account);
        match header_field {
            "subject" => draft.subject = injected.into(),
            "in-reply-to" => draft.in_reply_to = Some(injected.into()),
            "references" => draft.references = Some(injected.into()),
            _ => unreachable!(),
        }
        assert!(
            build_compose_message(&account, &draft).is_err(),
            "{header_field} accepted an injected header value"
        );
    }

    let mut display_name = account.clone();
    display_name.display_name = injected.into();
    assert!(
        build_compose_message(&display_name, &protocol_test_draft(&display_name)).is_err(),
        "display name accepted an injected header value"
    );

    let mut attachment = protocol_test_draft(&account);
    attachment.attachments.push(ComposeAttachment {
        filename: "invoice.pdf\r\nX-Dakia-Injection: filename".to_owned(),
        mime_type: "application/pdf".into(),
        content_base64: STANDARD.encode(b"fictional attachment"),
    });
    let wire = String::from_utf8(
        build_compose_message(&account, &attachment)
            .unwrap()
            .formatted(),
    )
    .unwrap();
    assert!(!wire.contains("\r\nX-Dakia-Injection:"));

    attachment.attachments[0].mime_type =
        "application/pdf\r\nX-Dakia-Injection: content-type".into();
    let wire = String::from_utf8(
        build_compose_message(&account, &attachment)
            .unwrap()
            .formatted(),
    )
    .unwrap();
    assert!(!wire.contains("\r\nX-Dakia-Injection:"));
    assert!(wire.contains("Content-Type: application/octet-stream"));
}

#[test]
fn outbound_header_control_character_matrix_is_rejected_before_formatting() {
    let account = test_account();
    for control in ['\0', '\u{1}', '\u{8}', '\u{b}', '\u{c}', '\u{1f}', '\u{7f}'] {
        for header_field in ["display-name", "subject", "in-reply-to", "references"] {
            let mut test_account = account.clone();
            let mut draft = protocol_test_draft(&test_account);
            let value = format!("safe{control}value");
            match header_field {
                "display-name" => test_account.display_name = value,
                "subject" => draft.subject = value,
                "in-reply-to" => draft.in_reply_to = Some(value),
                "references" => draft.references = Some(value),
                _ => unreachable!(),
            }
            assert!(
                build_compose_message(&test_account, &draft).is_err(),
                "{header_field} accepted U+{:04X}",
                control as u32
            );
        }
        for recipient_field in ["to", "cc", "bcc"] {
            let mut draft = protocol_test_draft(&account);
            let value = format!("Fictional{control}Recipient <recipient@example.test>");
            match recipient_field {
                "to" => draft.to = vec![value],
                "cc" => draft.cc = vec![value],
                "bcc" => draft.bcc = vec![value],
                _ => unreachable!(),
            }
            assert!(
                build_compose_message(&account, &draft).is_err(),
                "{recipient_field} accepted U+{:04X}",
                control as u32
            );
        }
    }
}

#[test]
fn horizontal_tab_remains_valid_header_whitespace_without_creating_a_field() {
    let mut account = test_account();
    account.display_name = "Fictional\tSender".into();
    let mut draft = protocol_test_draft(&account);
    draft.subject = "Fictional\tSubject".into();
    draft.in_reply_to = Some("<first@example.test>\t<second@example.test>".into());
    draft.references = Some("<root@example.test>\t<first@example.test>".into());

    let wire =
        String::from_utf8(build_compose_message(&account, &draft).unwrap().formatted()).unwrap();

    assert!(!wire.contains("\r\nX-Dakia-Injection:"));
    assert!(wire.contains("Subject:"));
}

#[tokio::test]
async fn production_imap_connector_bounds_tls_handshake_and_greeting_waits() {
    let handshake_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let handshake_account = imap_test_account(
        Security::Tls,
        handshake_listener.local_addr().unwrap().port(),
    );
    let handshake_server = tokio::spawn(async move {
        let (_stream, _) = handshake_listener.accept().await.unwrap();
        sleep(Duration::from_millis(200)).await;
    });
    let error = match ImapClient::connect_with_endpoint_and_timeouts(
        &handshake_account,
        imap_test_endpoint(handshake_account.imap_port, true),
        ImapConnectionTimeouts {
            connect: SHORT_DEADLINE,
            command: SHORT_DEADLINE,
        },
    )
    .await
    {
        Ok(_) => panic!("a silent TLS peer unexpectedly connected"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("TLS handshake timed out"),
        "{error:#}"
    );
    handshake_server.await.unwrap();

    let greeting_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let greeting_account = imap_test_account(
        Security::Tls,
        greeting_listener.local_addr().unwrap().port(),
    );
    let acceptor = smtp_test_acceptor();
    let greeting_server = tokio::spawn(async move {
        let (stream, _) = greeting_listener.accept().await.unwrap();
        let _stream = acceptor.accept(stream).await.unwrap();
        sleep(Duration::from_millis(200)).await;
    });
    let error = match ImapClient::connect_with_endpoint_and_timeouts(
        &greeting_account,
        imap_test_endpoint(greeting_account.imap_port, true),
        ImapConnectionTimeouts {
            connect: Duration::from_secs(1),
            command: SHORT_DEADLINE,
        },
    )
    .await
    {
        Ok(_) => panic!("a TLS server without an IMAP greeting unexpectedly connected"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("greeting timed out"),
        "{error:#}"
    );
    greeting_server.await.unwrap();
}

#[tokio::test]
async fn production_imap_connector_accepts_case_insensitive_exact_ok_status_tokens() {
    let implicit_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let implicit_account = imap_test_account(
        Security::Tls,
        implicit_listener.local_addr().unwrap().port(),
    );
    let implicit_acceptor = smtp_test_acceptor();
    let implicit_server = tokio::spawn(async move {
        let (stream, _) = implicit_listener.accept().await.unwrap();
        let stream = implicit_acceptor.accept(stream).await.unwrap();
        let mut encrypted = BufReader::new(stream);
        smtp_reply(&mut encrypted, "* ok lowercase greeting\r\n").await;
        assert_eq!(smtp_command(&mut encrypted).await, "D0001 CAPABILITY\r\n");
        smtp_reply(
            &mut encrypted,
            "* CAPABILITY IMAP4rev1\r\nD0001 ok capability\r\n",
        )
        .await;
    });
    ImapClient::connect_with_endpoint(
        &implicit_account,
        imap_test_endpoint(implicit_account.imap_port, true),
    )
    .await
    .unwrap();
    implicit_server.await.unwrap();

    let starttls_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let starttls_account = imap_test_account(
        Security::StartTls,
        starttls_listener.local_addr().unwrap().port(),
    );
    let starttls_acceptor = smtp_test_acceptor();
    let starttls_server = tokio::spawn(async move {
        let (stream, _) = starttls_listener.accept().await.unwrap();
        let mut plaintext = BufReader::new(stream);
        smtp_reply(&mut plaintext, "* oK lowercase greeting\r\n").await;
        assert_eq!(smtp_command(&mut plaintext).await, "D0000 STARTTLS\r\n");
        smtp_reply(&mut plaintext, "D0000 ok begin TLS\r\n").await;
        let stream = starttls_acceptor
            .accept(plaintext.into_inner())
            .await
            .unwrap();
        let mut encrypted = BufReader::new(stream);
        assert_eq!(smtp_command(&mut encrypted).await, "D0001 CAPABILITY\r\n");
        smtp_reply(
            &mut encrypted,
            "* CAPABILITY IMAP4rev1\r\nD0001 Ok capability\r\n",
        )
        .await;
    });
    ImapClient::connect_with_endpoint(
        &starttls_account,
        imap_test_endpoint(starttls_account.imap_port, true),
    )
    .await
    .unwrap();
    starttls_server.await.unwrap();
}

#[tokio::test]
async fn production_imap_connector_rejects_okay_as_a_status_token_at_every_upgrade_stage() {
    let implicit_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let implicit_account = imap_test_account(
        Security::Tls,
        implicit_listener.local_addr().unwrap().port(),
    );
    let implicit_acceptor = smtp_test_acceptor();
    let implicit_server = tokio::spawn(async move {
        let (stream, _) = implicit_listener.accept().await.unwrap();
        let stream = implicit_acceptor.accept(stream).await.unwrap();
        let mut encrypted = BufReader::new(stream);
        smtp_reply(&mut encrypted, "* OKAY is not OK\r\n").await;
        let mut forbidden = String::new();
        match timeout(
            Duration::from_millis(300),
            encrypted.read_line(&mut forbidden),
        )
        .await
        {
            Ok(Ok(0)) | Ok(Err(_)) => {}
            Ok(Ok(_)) => panic!("CAPABILITY followed an invalid implicit greeting: {forbidden:?}"),
            Err(_) => panic!("client kept an invalid implicit greeting open"),
        }
    });
    let error = match ImapClient::connect_with_endpoint(
        &implicit_account,
        imap_test_endpoint(implicit_account.imap_port, true),
    )
    .await
    {
        Ok(_) => panic!("implicit TLS accepted OKAY as OK"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("rejected connection"));
    implicit_server.await.unwrap();

    for invalid_stage in ["greeting", "starttls-response"] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let account = imap_test_account(Security::StartTls, listener.local_addr().unwrap().port());
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut plaintext = BufReader::new(stream);
            if invalid_stage == "greeting" {
                smtp_reply(&mut plaintext, "* OKAY is not OK\r\n").await;
            } else {
                smtp_reply(&mut plaintext, "* OK ready\r\n").await;
                assert_eq!(smtp_command(&mut plaintext).await, "D0000 STARTTLS\r\n");
                smtp_reply(&mut plaintext, "D0000 OKAY is not OK\r\n").await;
            }
            let mut forbidden = String::new();
            match timeout(
                Duration::from_millis(300),
                plaintext.read_line(&mut forbidden),
            )
            .await
            {
                Ok(Ok(0)) | Ok(Err(_)) => {}
                Ok(Ok(_)) => {
                    panic!("protocol work followed invalid STARTTLS {invalid_stage}: {forbidden:?}")
                }
                Err(_) => panic!("client kept invalid STARTTLS {invalid_stage} open"),
            }
        });
        let error = match ImapClient::connect_with_endpoint(
            &account,
            imap_test_endpoint(account.imap_port, true),
        )
        .await
        {
            Ok(_) => panic!("STARTTLS {invalid_stage} accepted OKAY as OK"),
            Err(error) => error,
        };
        let expected = if invalid_stage == "greeting" {
            "rejected connection"
        } else {
            "rejected STARTTLS"
        };
        assert!(error.to_string().contains(expected), "{error:#}");
        server.await.unwrap();
    }
}

#[tokio::test]
async fn starttls_imap_connector_bounds_plaintext_greeting_and_upgrade_response_waits() {
    for stage in ["greeting", "starttls-response"] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let account = imap_test_account(Security::StartTls, listener.local_addr().unwrap().port());
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut connection = BufReader::new(stream);
            if stage == "starttls-response" {
                smtp_reply(&mut connection, "* OK plaintext greeting\r\n").await;
                assert_eq!(smtp_command(&mut connection).await, "D0000 STARTTLS\r\n");
            }
            sleep(Duration::from_millis(200)).await;
        });
        let error = match ImapClient::connect_with_endpoint_and_timeouts(
            &account,
            imap_test_endpoint(account.imap_port, true),
            ImapConnectionTimeouts {
                connect: Duration::from_secs(1),
                command: SHORT_DEADLINE,
            },
        )
        .await
        {
            Ok(_) => panic!("silent STARTTLS {stage} unexpectedly connected"),
            Err(error) => error,
        };
        let expected = if stage == "greeting" {
            "IMAP greeting timed out"
        } else {
            "IMAP STARTTLS response timed out"
        };
        assert!(error.to_string().contains(expected), "{stage}: {error:#}");
        server.await.unwrap();
    }
}

#[tokio::test]
async fn production_imap_connector_bounds_post_tls_capability_wait_before_credentials() {
    for starttls in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let security = if starttls {
            Security::StartTls
        } else {
            Security::Tls
        };
        let account = imap_test_account(security, listener.local_addr().unwrap().port());
        let acceptor = smtp_test_acceptor();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let stream = if starttls {
                let mut plaintext = BufReader::new(stream);
                smtp_reply(&mut plaintext, "* OK plaintext greeting\r\n").await;
                assert_eq!(smtp_command(&mut plaintext).await, "D0000 STARTTLS\r\n");
                smtp_reply(&mut plaintext, "D0000 OK begin TLS\r\n").await;
                acceptor.accept(plaintext.into_inner()).await.unwrap()
            } else {
                acceptor.accept(stream).await.unwrap()
            };
            let mut encrypted = BufReader::new(stream);
            if !starttls {
                smtp_reply(&mut encrypted, "* OK encrypted greeting\r\n").await;
            }
            assert_eq!(smtp_command(&mut encrypted).await, "D0001 CAPABILITY\r\n");

            // Stay silent past the command deadline. The client must close
            // without exposing a password or OAuth bearer token.
            sleep(Duration::from_millis(200)).await;
            let mut forbidden = String::new();
            match timeout(
                Duration::from_millis(200),
                encrypted.read_line(&mut forbidden),
            )
            .await
            {
                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => {}
                Ok(Ok(_)) => panic!(
                    "credential command followed a timed-out post-TLS CAPABILITY: {forbidden:?}"
                ),
            }
        });

        let error = match ImapClient::connect_with_endpoint_and_timeouts(
            &account,
            imap_test_endpoint(account.imap_port, true),
            ImapConnectionTimeouts {
                connect: Duration::from_secs(1),
                command: SHORT_DEADLINE,
            },
        )
        .await
        {
            Ok(_) => panic!("silent post-TLS CAPABILITY unexpectedly connected"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("post-TLS capability check timed out"),
            "{error:#}"
        );
        assert!(
            error.root_cause().is::<tokio::time::error::Elapsed>(),
            "timeout context lost its typed source: {error:#}"
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn imap_probe_checks_post_tls_capabilities_before_credentials() {
    let store = Store::in_memory().await.unwrap();
    let service = MailService::new(store);
    let account = test_account();
    service
        .credentials()
        .set_password(&account, "must-not-cross-wire")
        .await
        .unwrap();
    let (mut client, server) = plain_imap_client_and_server().await;
    let server = tokio::spawn(async move {
        let (read, mut write) = server.into_split();
        let mut read = BufReader::new(read);
        let (tag, command) = scripted_command(&mut read).await;
        assert_eq!(command, "CAPABILITY");
        write
            .write_all(
                format!(
                    "* CAPABILITY IMAP4rev1 LOGINDISABLED AUTH=PLAIN\r\n{tag} OK capability\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        write.flush().await.unwrap();

        let mut forbidden = String::new();
        match timeout(Duration::from_millis(200), read.read_line(&mut forbidden)).await {
            Ok(Ok(0)) | Err(_) => {}
            Ok(Ok(_)) => {
                panic!("credential command sent despite capability mismatch: {forbidden:?}")
            }
            Ok(Err(_)) => {}
        }
    });

    let error = service
        .imap_auth_probe_with_client(&mut client, &account)
        .await
        .expect_err("LOGINDISABLED must fail before LOGIN");
    assert!(
        error.to_string().contains("does not permit password LOGIN"),
        "{error:#}"
    );
    drop(client);
    server.await.unwrap();

    let mut oauth = test_account();
    oauth.auth = AccountAuth::OAuth2 {
        username: "reader@example.test".into(),
        provider: "fixture".into(),
        access_token_expires_at: None,
    };
    assert!(
        validate_imap_auth_capabilities(&oauth, &["* CAPABILITY IMAP4rev1 AUTH=PLAIN".into()])
            .unwrap_err()
            .to_string()
            .contains("AUTH=XOAUTH2")
    );
    assert!(validate_imap_auth_capabilities(
        &oauth,
        &["* CAPABILITY IMAP4rev1 AUTH=XOAUTH2".into()]
    )
    .unwrap_err()
    .to_string()
    .contains("SASL-IR"));
    validate_imap_auth_capabilities(
        &oauth,
        &["* capability IMAP4rev1 auth=xoauth2 sasl-ir".into()],
    )
    .unwrap();
}

#[tokio::test]
async fn starttls_imap_probe_uses_only_post_tls_capabilities_for_authentication() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let account = imap_test_account(Security::StartTls, listener.local_addr().unwrap().port());
    let acceptor = smtp_test_acceptor();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut plaintext = BufReader::new(stream);
        smtp_reply(&mut plaintext, "* OK plaintext greeting\r\n").await;
        assert_eq!(smtp_command(&mut plaintext).await, "D0000 STARTTLS\r\n");
        smtp_reply(&mut plaintext, "D0000 OK begin TLS\r\n").await;

        let stream = acceptor.accept(plaintext.into_inner()).await.unwrap();
        let mut encrypted = BufReader::new(stream);
        assert_eq!(smtp_command(&mut encrypted).await, "D0001 CAPABILITY\r\n");
        smtp_reply(
            &mut encrypted,
            "* CAPABILITY IMAP4rev1 LOGINDISABLED AUTH=PLAIN\r\nD0001 OK capability\r\n",
        )
        .await;
        let mut forbidden = String::new();
        match timeout(
            Duration::from_millis(200),
            encrypted.read_line(&mut forbidden),
        )
        .await
        {
            Ok(Ok(0)) | Err(_) => {}
            Ok(Ok(_)) => panic!("authentication ignored post-TLS capabilities: {forbidden:?}"),
            Ok(Err(_)) => {}
        }
    });

    let error = match ImapClient::connect_with_endpoint(
        &account,
        imap_test_endpoint(account.imap_port, true),
    )
    .await
    {
        Ok(_) => panic!("post-TLS LOGINDISABLED unexpectedly connected"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("does not permit password LOGIN"));
    server.await.unwrap();
}

#[tokio::test]
async fn production_imap_connector_rejects_missing_xoauth2_before_authentication() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
    account.auth = AccountAuth::OAuth2 {
        username: "reader@example.test".into(),
        provider: "fictional".into(),
        access_token_expires_at: None,
    };
    let acceptor = smtp_test_acceptor();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = acceptor.accept(stream).await.unwrap();
        let mut encrypted = BufReader::new(stream);
        smtp_reply(&mut encrypted, "* OK fictional IMAP ready\r\n").await;
        assert_eq!(smtp_command(&mut encrypted).await, "D0001 CAPABILITY\r\n");
        smtp_reply(
            &mut encrypted,
            "* CAPABILITY IMAP4rev1 AUTH=PLAIN\r\nD0001 OK capability\r\n",
        )
        .await;
        let mut forbidden = String::new();
        match timeout(
            Duration::from_millis(200),
            encrypted.read_line(&mut forbidden),
        )
        .await
        {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => {}
            Ok(Ok(_)) => panic!("XOAUTH2 command sent without advertised support: {forbidden:?}"),
        }
    });

    let error = match ImapClient::connect_with_endpoint(
        &account,
        imap_test_endpoint(account.imap_port, true),
    )
    .await
    {
        Ok(_) => panic!("missing AUTH=XOAUTH2 unexpectedly connected"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("AUTH=XOAUTH2"));
    server.await.unwrap();
}

#[tokio::test]
async fn production_imap_connector_rejects_tagged_only_capability_before_login() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
    let acceptor = smtp_test_acceptor();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = acceptor.accept(stream).await.unwrap();
        let mut encrypted = BufReader::new(stream);
        smtp_reply(&mut encrypted, "* OK fictional IMAP ready\r\n").await;
        assert_eq!(smtp_command(&mut encrypted).await, "D0001 CAPABILITY\r\n");
        smtp_reply(&mut encrypted, "D0001 OK capability omitted\r\n").await;
        let mut forbidden = String::new();
        match timeout(
            Duration::from_millis(200),
            encrypted.read_line(&mut forbidden),
        )
        .await
        {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => {}
            Ok(Ok(_)) => panic!("LOGIN followed a missing CAPABILITY response: {forbidden:?}"),
        }
    });

    let error = match ImapClient::connect_with_endpoint(
        &account,
        imap_test_endpoint(account.imap_port, true),
    )
    .await
    {
        Ok(_) => panic!("tagged-only CAPABILITY unexpectedly connected"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("supported IMAP profile"));
    server.await.unwrap();

    assert!(validate_imap_auth_capabilities(
        &account,
        &["* CAPABILITY".into(), "D0001 OK capability".into()]
    )
    .is_err());
}

#[tokio::test]
async fn production_imap_connector_rejects_malformed_capability_atom_before_login() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
    let acceptor = smtp_test_acceptor();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = acceptor.accept(stream).await.unwrap();
        let mut encrypted = BufReader::new(stream);
        smtp_reply(&mut encrypted, "* OK fictional IMAP ready\r\n").await;
        assert_eq!(smtp_command(&mut encrypted).await, "D0001 CAPABILITY\r\n");
        smtp_reply(
            &mut encrypted,
            "* CAPABILITY IMAP4rev1 LOGINDISABLED]\r\nD0001 OK capability\r\n",
        )
        .await;
        let mut forbidden = String::new();
        match timeout(
            Duration::from_millis(200),
            encrypted.read_line(&mut forbidden),
        )
        .await
        {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => {}
            Ok(Ok(_)) => panic!("LOGIN followed a malformed capability atom: {forbidden:?}"),
        }
    });

    let error = match ImapClient::connect_with_endpoint(
        &account,
        imap_test_endpoint(account.imap_port, true),
    )
    .await
    {
        Ok(_) => panic!("malformed capability atom unexpectedly connected"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("malformed capability response"),
        "{error:#}"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn production_imap_connector_rejects_xoauth2_without_sasl_ir() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut account = imap_test_account(Security::Tls, listener.local_addr().unwrap().port());
    account.auth = AccountAuth::OAuth2 {
        username: "reader@example.test".into(),
        provider: "fictional".into(),
        access_token_expires_at: None,
    };
    let acceptor = smtp_test_acceptor();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = acceptor.accept(stream).await.unwrap();
        let mut encrypted = BufReader::new(stream);
        smtp_reply(&mut encrypted, "* OK fictional IMAP ready\r\n").await;
        assert_eq!(smtp_command(&mut encrypted).await, "D0001 CAPABILITY\r\n");
        smtp_reply(
            &mut encrypted,
            "* CAPABILITY IMAP4rev1 AUTH=XOAUTH2\r\nD0001 OK capability\r\n",
        )
        .await;
        let mut forbidden = String::new();
        match timeout(
            Duration::from_millis(200),
            encrypted.read_line(&mut forbidden),
        )
        .await
        {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => {}
            Ok(Ok(_)) => panic!("AUTHENTICATE was sent without SASL-IR: {forbidden:?}"),
        }
    });

    let error = match ImapClient::connect_with_endpoint(
        &account,
        imap_test_endpoint(account.imap_port, true),
    )
    .await
    {
        Ok(_) => panic!("XOAUTH2 without SASL-IR unexpectedly connected"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("SASL-IR"));
    server.await.unwrap();
}

#[tokio::test]
async fn imap_protocol_fault_matrix_does_not_return_untrusted_literals() {
    let (mut wrong_tag_client, wrong_tag_server) = plain_imap_client_and_server().await;
    let wrong_tag_server = tokio::spawn(async move {
        let (read, mut write) = wrong_tag_server.into_split();
        let mut read = BufReader::new(read);
        let (_, command) = scripted_command(&mut read).await;
        assert_eq!(command, "NOOP");
        write.write_all(b"D9999 OK wrong tag\r\n").await.unwrap();
    });
    assert!(wrong_tag_client
        .command("NOOP")
        .await
        .unwrap_err()
        .to_string()
        .contains("connection closed"));
    wrong_tag_server.await.unwrap();

    let (mut truncated_client, truncated_server) = plain_imap_client_and_server().await;
    let truncated_server = tokio::spawn(async move {
        let (read, mut write) = truncated_server.into_split();
        let mut read = BufReader::new(read);
        let (_, command) = scripted_command(&mut read).await;
        assert_eq!(command, "UID FETCH 42 (BODY.PEEK[1])");
        write
            .write_all(b"* 1 FETCH (UID 42 BODY[1] {5}\r\nabc")
            .await
            .unwrap();
    });
    assert!(truncated_client
        .command_with_literal("UID FETCH 42 (BODY.PEEK[1])")
        .await
        .is_err());
    truncated_server.await.unwrap();

    let (mut malformed_client, malformed_server) = plain_imap_client_and_server().await;
    let malformed_server = tokio::spawn(async move {
        let (read, mut write) = malformed_server.into_split();
        let mut read = BufReader::new(read);
        let (tag, command) = scripted_command(&mut read).await;
        assert_eq!(command, "UID FETCH 42 (BODY.PEEK[1])");
        write
            .write_all(
                format!("* 1 FETCH (UID 42 BODY[1] {{invalid}}\r\n{tag} OK complete\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
    });
    let mut response = malformed_client
        .command_with_literal("UID FETCH 42 (BODY.PEEK[1])")
        .await
        .unwrap();
    assert!(response.take_body_literal_for(42, "1").is_none());
    malformed_server.await.unwrap();

    assert_eq!(
        parse_imap_astring(r#""A\"B" remainder"#),
        Some(("A\"B".into(), " remainder"))
    );
    assert_eq!(
        parse_imap_astring("NIL remainder"),
        Some(("NIL".into(), " remainder"))
    );
}

#[tokio::test]
async fn imap_idle_renews_with_done_and_a_matching_tag() {
    let (mut client, server) = plain_imap_client_and_server().await;
    let server = tokio::spawn(async move {
        let (read, mut write) = server.into_split();
        let mut read = BufReader::new(read);
        let (tag, command) = scripted_command(&mut read).await;
        assert_eq!(command, "IDLE");
        write.write_all(b"+ idling\r\n").await.unwrap();
        write.flush().await.unwrap();
        let mut done = String::new();
        read.read_line(&mut done).await.unwrap();
        assert_eq!(done, "DONE\r\n");
        write
            .write_all(format!("{tag} OK IDLE renewed\r\n").as_bytes())
            .await
            .unwrap();
    });
    let (_sender, mut active) = watch::channel(false);
    assert!(matches!(
        client
            .idle_until_change_with_renewal(&mut active, SHORT_DEADLINE)
            .await
            .unwrap(),
        IdleOutcome::Renewed
    ));
    server.await.unwrap();
}

#[tokio::test]
async fn smtp_disconnect_and_final_reply_matrix_preserves_delivery_certainty() {
    for stage in [
        "before-data",
        "during-upload",
        "final-transient-rejection",
        "final-permanent-rejection",
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let account = smtp_test_account(Security::Tls, listener.local_addr().unwrap().port());
        let acceptor = smtp_test_acceptor();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let stream = acceptor.accept(stream).await.unwrap();
            let mut connection = BufReader::new(stream);
            smtp_reply(&mut connection, "220 localhost ready\r\n").await;
            smtp_expect_ehlo_and_auth(&mut connection, false).await;
            if stage == "before-data" {
                assert!(smtp_command(&mut connection)
                    .await
                    .starts_with("MAIL FROM:"));
                return;
            }
            smtp_expect_envelope_until_data(&mut connection).await;
            if stage == "during-upload" {
                let first_line = smtp_command(&mut connection).await;
                assert!(first_line.starts_with("From:"));
                return;
            }
            let _message = smtp_read_message(&mut connection).await;
            let reply = if stage == "final-transient-rejection" {
                "451 4.3.0 final processing failed\r\n"
            } else {
                "554 5.7.1 final content rejected\r\n"
            };
            smtp_reply(&mut connection, reply).await;
        });

        let error = send_smtp_raw(
            &account,
            &smtp_test_envelope(),
            b"From: sender@example.test\r\nTo: recipient@example.test\r\nSubject: staged failure\r\n\r\nbody\r\n",
            "secret",
            smtp_test_endpoint(&account),
            Duration::from_secs(2),
        )
        .await
        .expect_err(stage);
        server.await.unwrap();
        let expected = if stage == "during-upload" {
            SendFailureKind::DeliveryUncertain
        } else {
            SendFailureKind::NotSubmitted
        };
        assert_eq!(
            error.downcast_ref::<SendFailure>().unwrap().kind,
            expected,
            "{stage}"
        );
    }
}

#[tokio::test]
async fn sent_append_timeout_after_smtp_acceptance_is_sent_copy_failed() {
    let smtp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let imap_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut account = smtp_test_account(Security::Tls, smtp_listener.local_addr().unwrap().port());
    account.imap_host = "127.0.0.1".into();
    account.imap_port = imap_listener.local_addr().unwrap().port();
    account.imap_security = Security::Tls;

    let acceptor = smtp_test_acceptor();
    let smtp_server = tokio::spawn(async move {
        let (stream, _) = smtp_listener.accept().await.unwrap();
        let stream = acceptor.accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        smtp_reply(&mut connection, "220 localhost ready\r\n").await;
        smtp_expect_ehlo_and_auth(&mut connection, false).await;
        smtp_expect_envelope_until_data(&mut connection).await;
        let _message = smtp_read_message(&mut connection).await;
        smtp_reply(&mut connection, "250 2.0.0 accepted exactly once\r\n").await;
    });

    let imap_acceptor = smtp_test_acceptor();
    let imap_server = tokio::spawn(async move {
        let (stream, _) = imap_listener.accept().await.unwrap();
        let stream = imap_acceptor.accept(stream).await.unwrap();
        let mut connection = BufReader::new(stream);
        smtp_reply(&mut connection, "* OK loopback IMAP ready\r\n").await;
        assert_eq!(smtp_command(&mut connection).await, "D0001 CAPABILITY\r\n");
        smtp_reply(
            &mut connection,
            "* CAPABILITY IMAP4rev1\r\nD0001 OK capability\r\n",
        )
        .await;
        assert!(smtp_command(&mut connection).await.contains(" LOGIN "));
        smtp_reply(&mut connection, "D0002 OK logged in\r\n").await;
        assert_eq!(
            smtp_command(&mut connection).await,
            "D0003 LIST \"\" \"*\"\r\n"
        );
        smtp_reply(
            &mut connection,
            "* LIST (\\Sent) \"/\" \"Sent\"\r\nD0003 OK listed\r\n",
        )
        .await;
        let append = smtp_command(&mut connection).await;
        assert!(append.starts_with("D0004 APPEND \"Sent\" (\\Seen) {"));
        smtp_reply(&mut connection, "+ continue\r\n").await;
        sleep(Duration::from_millis(250)).await;
    });

    let service = MailService::new(Store::in_memory().await.unwrap());
    let draft = ComposeMessage {
        account_id: account.id,
        to: vec!["recipient@example.test".into()],
        cc: vec![],
        bcc: vec![],
        subject: "Sent APPEND timeout".into(),
        body_text: "accepted by SMTP".into(),
        body_html: None,
        in_reply_to: None,
        references: None,
        attachments: vec![],
    };
    let error = service
        .send_with_endpoints(
            &account,
            &draft,
            "secret",
            smtp_test_endpoint(&account),
            Some(imap_test_endpoint(account.imap_port, true)),
            Duration::from_millis(150),
        )
        .await
        .expect_err("Sent APPEND must time out after SMTP acceptance");
    assert_eq!(
        error.downcast_ref::<SendFailure>().unwrap().kind,
        SendFailureKind::SentCopyFailed
    );
    smtp_server.await.unwrap();
    imap_server.await.unwrap();
}
