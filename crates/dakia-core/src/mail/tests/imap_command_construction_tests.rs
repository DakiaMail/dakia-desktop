use super::*;
use tokio::{
    io::{duplex, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream},
    time::timeout,
};

fn imap_pair() -> (ImapClient<DuplexStream>, DuplexStream) {
    let (client, server) = duplex(8 * 1024);
    (
        ImapClient {
            reader: BufReader::new(client),
            tag: 0,
        },
        server,
    )
}

fn auth_account(auth: AccountAuth) -> Account {
    Account {
        id: uuid::Uuid::new_v4(),
        email: "reader@example.test".into(),
        account_name: "Framing fixture".into(),
        display_name: "Reader".into(),
        provider_id: "custom".into(),
        auth,
        imap_host: "imap.example.test".into(),
        imap_port: 993,
        imap_security: Security::Tls,
        smtp_host: "smtp.example.test".into(),
        smtp_port: 465,
        smtp_security: Security::Tls,
        archive_mailbox: "Archive".into(),
        spam_mailbox: "Spam".into(),
        enabled: true,
        created_at: Utc::now(),
    }
}

async fn assert_no_wire_bytes(server: &mut DuplexStream) {
    let mut byte = [0u8; 1];
    assert!(
        timeout(Duration::from_millis(20), server.read(&mut byte))
            .await
            .is_err(),
        "rejected IMAP input wrote bytes to the transport"
    );
}

#[tokio::test]
async fn tagged_commands_reject_framing_bytes_before_any_wire_write() {
    for injected in [
        "UID SEARCH TEXT \"safe\r\nD9999 LOGOUT\"",
        "UID SEARCH TEXT \"safe\nD9999 LOGOUT\"",
        "UID SEARCH TEXT \"safe\0tail\"",
    ] {
        let (mut client, mut server) = imap_pair();
        let error = timeout(Duration::from_millis(50), client.command(injected))
            .await
            .expect("framing rejection must not wait for an IMAP response")
            .expect_err("framing bytes must be rejected");

        assert_eq!(client.tag, 0, "rejected command consumed a tag");
        assert!(
            !error.to_string().contains(injected),
            "rejection exposed untrusted command input"
        );
        assert_no_wire_bytes(&mut server).await;
    }
}

#[tokio::test]
async fn password_authentication_rejects_framing_bytes_without_exposing_credentials() {
    for (username, secret) in [
        ("reader@example.test\r\nD9999 LOGOUT", "safe secret"),
        ("reader@example.test", "safe\nD9999 LOGOUT"),
        ("reader@example.test", "safe\0tail"),
    ] {
        let (mut client, mut server) = imap_pair();
        let account = auth_account(AccountAuth::Password {
            username: username.into(),
        });

        let error = timeout(
            Duration::from_millis(50),
            client.authenticate(&account, secret),
        )
        .await
        .expect("credential framing rejection must not wait for an IMAP response")
        .expect_err("credential framing bytes must be rejected");

        let description = error.to_string();
        assert_eq!(client.tag, 0, "rejected LOGIN consumed a tag");
        assert!(!description.contains(username));
        assert!(!description.contains(secret));
        assert_no_wire_bytes(&mut server).await;
    }
}

#[tokio::test]
async fn oauth_authentication_rejects_raw_framing_bytes_before_base64_encoding() {
    for (username, secret) in [
        ("reader@example.test\r\nD9999 LOGOUT", "safe token"),
        ("reader@example.test", "safe\nD9999 LOGOUT"),
        ("reader@example.test", "safe\0tail"),
    ] {
        let (mut client, mut server) = imap_pair();
        let account = auth_account(AccountAuth::OAuth2 {
            username: username.into(),
            provider: "fixture".into(),
            access_token_expires_at: None,
        });

        let error = timeout(
            Duration::from_millis(50),
            client.authenticate(&account, secret),
        )
        .await
        .expect("OAuth framing rejection must not wait for an IMAP response")
        .expect_err("raw OAuth framing bytes must be rejected");

        let description = error.to_string();
        assert_eq!(client.tag, 0, "rejected XOAUTH2 consumed a tag");
        assert!(!description.contains(username));
        assert!(!description.contains(secret));
        assert_no_wire_bytes(&mut server).await;
    }
}

#[tokio::test]
async fn authentication_rejection_does_not_retain_an_echoed_secret_in_the_error_chain() {
    for status in ["NO", "BAD"] {
        let (mut client, server) = imap_pair();
        let account = auth_account(AccountAuth::Password {
            username: "reader@example.test".into(),
        });
        let secret = "private fixture secret";
        let response = format!("D0001 {status} private fixture secret was rejected\r\n");
        let server = tokio::spawn(async move {
            let mut server = BufReader::new(server);
            let mut command = String::new();
            server.read_line(&mut command).await.unwrap();
            assert_eq!(
                command,
                "D0001 LOGIN \"reader@example.test\" \"private fixture secret\"\r\n"
            );
            server
                .get_mut()
                .write_all(response.as_bytes())
                .await
                .unwrap();
        });

        let error = client.authenticate(&account, secret).await.unwrap_err();
        server.await.unwrap();
        assert_eq!(error.to_string(), "IMAP authentication rejected");
        assert!(!format!("{error:?}").contains(secret));
        assert!(error
            .chain()
            .all(|cause| !cause.to_string().contains(secret)));
    }
}

#[tokio::test]
async fn authentication_bye_does_not_retain_echoed_credentials_in_retryable_errors() {
    let password_secret = "private transport secret";
    let (mut password_client, password_server) = imap_pair();
    let password_account = auth_account(AccountAuth::Password {
        username: "reader@example.test".into(),
    });
    let password_server = tokio::spawn(async move {
        let mut server = BufReader::new(password_server);
        let mut command = String::new();
        server.read_line(&mut command).await.unwrap();
        server
            .get_mut()
            .write_all(b"* BYE private transport secret was echoed\r\n")
            .await
            .unwrap();
    });
    let password_error = password_client
        .authenticate(&password_account, password_secret)
        .await
        .unwrap_err();
    password_server.await.unwrap();
    assert_eq!(
        password_error.to_string(),
        "IMAP connection failed during credential exchange"
    );
    assert!(!format!("{password_error:?}").contains(password_secret));
    assert!(password_error
        .chain()
        .all(|cause| !cause.to_string().contains(password_secret)));

    let oauth_secret = "private OAuth token";
    let oauth_account = auth_account(AccountAuth::OAuth2 {
        username: "reader@example.test".into(),
        provider: "fixture".into(),
        access_token_expires_at: None,
    });
    let encoded = STANDARD.encode(format!(
        "user={}\x01auth=Bearer {oauth_secret}\x01\x01",
        oauth_account.auth.username()
    ));
    let echoed = encoded.clone();
    let (mut oauth_client, oauth_server) = imap_pair();
    let oauth_server = tokio::spawn(async move {
        let mut server = BufReader::new(oauth_server);
        let mut command = String::new();
        server.read_line(&mut command).await.unwrap();
        assert!(command.contains(&echoed));
        server
            .get_mut()
            .write_all(format!("* BYE {echoed}\r\n").as_bytes())
            .await
            .unwrap();
    });
    let oauth_error = oauth_client
        .authenticate(&oauth_account, oauth_secret)
        .await
        .unwrap_err();
    oauth_server.await.unwrap();
    assert_eq!(
        oauth_error.to_string(),
        "IMAP connection failed during credential exchange"
    );
    let debug = format!("{oauth_error:?}");
    assert!(!debug.contains(oauth_secret));
    assert!(!debug.contains(&encoded));
    assert!(oauth_error.chain().all(|cause| {
        let description = cause.to_string();
        !description.contains(oauth_secret) && !description.contains(&encoded)
    }));
}

#[tokio::test]
async fn generic_command_rejections_do_not_retain_provider_echoes() {
    for (response, expected) in [
        (
            "D0001 NO private-command-input was rejected\r\n",
            "IMAP command failed: NO",
        ),
        (
            "D0001 BAD private-command-input was malformed\r\n",
            "IMAP command failed: BAD",
        ),
        (
            "D0001 NO [NONEXISTENT] private-command-input was echoed\r\n",
            "IMAP command failed: NO [NONEXISTENT]",
        ),
        (
            "D0001 NO [PRIVATE-CODE] private-command-input was echoed\r\n",
            "IMAP command failed: NO",
        ),
        (
            "D0001 BAD [NONEXISTENT] private-command-input was echoed\r\n",
            "IMAP command failed: BAD",
        ),
        (
            "* BYE private-command-input was echoed\r\n",
            "IMAP server closed the connection",
        ),
    ] {
        let (mut client, server) = imap_pair();
        let response = response.to_owned();
        let server = tokio::spawn(async move {
            let mut server = BufReader::new(server);
            let mut command = String::new();
            server.read_line(&mut command).await.unwrap();
            assert_eq!(command, "D0001 NOOP\r\n");
            server
                .get_mut()
                .write_all(response.as_bytes())
                .await
                .unwrap();
        });

        let error = client.command("NOOP").await.unwrap_err();
        server.await.unwrap();
        assert_eq!(error.to_string(), expected);
        assert!(!format!("{error:?}").contains("private-command-input"));
        assert!(error
            .chain()
            .all(|cause| !cause.to_string().contains("private-command-input")));
    }
}

#[tokio::test]
async fn append_rejections_do_not_retain_echoed_mailbox_or_message_content() {
    let mailbox = "Private Sent Mail";
    let message = b"Subject: private fixture\r\n\r\nprivate-message-body";

    for response in [
        "D0001 NO Private Sent Mail is unavailable\r\n",
        "* BYE Private Sent Mail was echoed\r\n",
    ] {
        let (mut client, server) = imap_pair();
        let response = response.to_owned();
        let server = tokio::spawn(async move {
            let mut server = BufReader::new(server);
            let mut prefix = String::new();
            server.read_line(&mut prefix).await.unwrap();
            server
                .get_mut()
                .write_all(response.as_bytes())
                .await
                .unwrap();
        });

        let error = client
            .append_with_timeout(mailbox, message, Duration::from_millis(200))
            .await
            .unwrap_err();
        server.await.unwrap();
        let debug = format!("{error:?}");
        assert!(!debug.contains(mailbox));
        assert!(!debug.contains("private-message-body"));
    }

    let (mut client, server) = imap_pair();
    let expected_length = message.len();
    let server = tokio::spawn(async move {
        let mut server = BufReader::new(server);
        let mut prefix = String::new();
        server.read_line(&mut prefix).await.unwrap();
        server.get_mut().write_all(b"+ continue\r\n").await.unwrap();
        let mut literal = vec![0; expected_length];
        server.read_exact(&mut literal).await.unwrap();
        let mut terminator = [0u8; 2];
        server.read_exact(&mut terminator).await.unwrap();
        server
            .get_mut()
            .write_all(b"D0001 BAD private-message-body was rejected\r\n")
            .await
            .unwrap();
    });
    let error = client
        .append_with_timeout(mailbox, message, Duration::from_millis(200))
        .await
        .unwrap_err();
    server.await.unwrap();
    let debug = format!("{error:?}");
    assert!(!debug.contains(mailbox));
    assert!(!debug.contains("private-message-body"));
}

#[tokio::test]
async fn append_rejects_mailbox_framing_bytes_before_prefix_or_literal_write() {
    let literal = b"Subject: fixture\r\n\r\nbody\0with framing bytes";
    for mailbox in [
        "INBOX\r\nD9999 LOGOUT",
        "INBOX\nD9999 LOGOUT",
        "INBOX\0tail",
    ] {
        let (mut client, mut server) = imap_pair();
        let error = client
            .append_with_timeout(mailbox, literal, Duration::from_millis(50))
            .await
            .expect_err("mailbox framing bytes must be rejected");

        assert_eq!(client.tag, 0, "rejected APPEND consumed a tag");
        assert!(
            !error.to_string().contains(mailbox),
            "rejection exposed untrusted mailbox input"
        );
        assert_no_wire_bytes(&mut server).await;
    }
}

#[tokio::test]
async fn append_preserves_raw_literal_bytes_and_quotes_a_valid_unicode_mailbox() {
    let mailbox = "雪 \"töö\" \\ path";
    let message = b"Subject: literal\r\n\r\nbody\0keeps\r\nframing bytes";
    let expected = message.to_vec();
    let (mut client, server) = imap_pair();
    let server = tokio::spawn(async move {
        let mut server = BufReader::new(server);
        let mut prefix = String::new();
        server.read_line(&mut prefix).await.unwrap();
        assert_eq!(
            prefix,
            format!(
                "D0001 APPEND \"\u{96ea} \\\"t\u{f6}\u{f6}\\\" \\\\ path\" (\\Seen) {{{}}}\r\n",
                expected.len()
            )
        );
        server.get_mut().write_all(b"+ continue\r\n").await.unwrap();

        let mut received = vec![0; expected.len()];
        server.read_exact(&mut received).await.unwrap();
        assert_eq!(received, expected);
        let mut terminator = [0u8; 2];
        server.read_exact(&mut terminator).await.unwrap();
        assert_eq!(&terminator, b"\r\n");
        server
            .get_mut()
            .write_all(b"D0001 OK appended\r\n")
            .await
            .unwrap();
    });

    client
        .append_with_timeout(mailbox, message, Duration::from_millis(200))
        .await
        .unwrap();
    server.await.unwrap();
}
