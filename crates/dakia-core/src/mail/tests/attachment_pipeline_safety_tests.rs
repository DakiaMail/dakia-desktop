use super::*;
use crate::{storage::AttachmentPresentation, CachedMessageContent};
use base64::{engine::general_purpose::STANDARD, Engine};
use tokio::{
    io::{split, AsyncWriteExt, BufReader},
    net::TcpListener,
};

#[tokio::test]
async fn governed_filename_fixture_preserves_safe_metadata_through_storage_and_ipc() {
    let account = test_account();
    let parsed = parse_message(
        &account,
        "INBOX",
        73,
        &[],
        include_bytes!("../../../testdata/mime/filename-parameters-and-encodings.eml"),
        None,
    )
    .await
    .expect("the governed filename corpus fixture must parse");

    let expected = parsed
        .attachments
        .iter()
        .map(|attachment| {
            (
                attachment.attachment.filename.clone(),
                attachment.attachment.mime_type.clone(),
                attachment.attachment.is_potentially_unsafe,
            )
        })
        .collect::<Vec<_>>();
    assert!(expected
        .iter()
        .any(|(filename, _, _)| { filename == "quarterly report final.pdf" }));
    assert!(expected
        .iter()
        .any(|(filename, _, _)| filename == "invoice;final.pdf"));
    assert!(expected.iter().any(|(filename, _, unsafe_attachment)| {
        filename == "invoicegpj.exe" && *unsafe_attachment
    }));
    assert!(expected.iter().all(|(filename, _, _)| {
        !filename.contains(['/', '\\'])
            && filename.chars().all(|character| {
                !character.is_control()
                    && !matches!(
                        character,
                        '\u{200e}'
                            | '\u{200f}'
                            | '\u{202a}'..='\u{202e}'
                            | '\u{2066}'..='\u{2069}'
                    )
            })
    }));

    let directory = tempfile::tempdir().expect("temporary profile directory");
    let database = directory.path().join("attachment-filename-boundary.sqlite");
    let store = Store::open(&database).await.expect("open profile");
    store.save_account(&account).await.expect("save account");
    store
        .upsert_messages(std::slice::from_ref(&parsed))
        .await
        .expect("persist parsed attachment metadata");
    store
        .cache_message_content(
            &parsed.id,
            parsed.is_flagged,
            CachedMessageContent {
                body_text: parsed.body_text.clone(),
                body_html: parsed.body_html.clone(),
                unsubscribe_kind: parsed.unsubscribe_kind.clone(),
                attachments: parsed
                    .attachments
                    .iter()
                    .map(|attachment| attachment.attachment.clone())
                    .collect(),
            },
        )
        .await
        .expect("cache Reader attachment metadata");
    let message_id = parsed.id.clone();
    drop(store);

    let reopened = Store::open(&database).await.expect("reopen profile");
    let cached = reopened
        .cached_message_content(&message_id)
        .await
        .expect("read cached content")
        .expect("cached attachment metadata");
    assert_eq!(
        cached
            .attachments
            .iter()
            .map(|attachment| {
                (
                    attachment.filename.clone(),
                    attachment.mime_type.clone(),
                    attachment.is_potentially_unsafe,
                )
            })
            .collect::<Vec<_>>(),
        expected
    );
    let ipc_payload = serde_json::to_value(&cached.attachments).expect("IPC attachment payload");
    assert_eq!(
        ipc_payload
            .as_array()
            .expect("attachment array")
            .iter()
            .find(|attachment| attachment["filename"] == "invoicegpj.exe")
            .expect("bidi-sanitized executable attachment")["is_potentially_unsafe"],
        true
    );
}

#[tokio::test]
async fn production_attachment_fetch_decodes_exact_binary_bytes_at_captured_epoch() {
    let store = Store::in_memory().await.unwrap();
    let service = MailService::new(store.clone());
    let account = test_account();
    store.save_account(&account).await.unwrap();
    service
        .credentials()
        .set_password(&account, "sync secret")
        .await
        .unwrap();
    store
        .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", 77, 1, true)
        .await
        .unwrap();

    // A synthetic binary attachment includes a redacted MIME corpus entry,
    // NUL, and invalid UTF-8. Its bytes must never pass through text decoding.
    let raw = [
        b"\0\xff\x80".as_slice(),
        include_bytes!("../../../testdata/mime/multipart-attachment-container.eml").as_slice(),
    ]
    .concat();
    assert!(std::str::from_utf8(&raw).is_err());
    let encoded = STANDARD.encode(&raw);
    let encoded_len = encoded.len();
    let headers = concat!(
        "Content-Type: application/octet-stream; name=\"invoice.command\"\r\n",
        "Content-Disposition: attachment; filename=\"invoice.command\"\r\n",
        "Content-Transfer-Encoding: base64\r\n\r\n"
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let acceptor = smtp_test_acceptor();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = acceptor.accept(stream).await.unwrap();
        let (read, mut write) = split(stream);
        let mut read = BufReader::new(read);
        write
            .write_all(b"* OK loopback IMAP ready\r\n")
            .await
            .unwrap();
        let tag = scripted_expect_command(&mut read, &mut Vec::new(), "CAPABILITY").await;
        write
            .write_all(format!("* CAPABILITY IMAP4rev1\r\n{tag} OK capability\r\n").as_bytes())
            .await
            .unwrap();
        let tag = scripted_expect_command(
            &mut read,
            &mut Vec::new(),
            "LOGIN \"reader@example.test\" \"sync secret\"",
        )
        .await;
        write
            .write_all(format!("{tag} OK authenticated\r\n").as_bytes())
            .await
            .unwrap();
        let tag = scripted_expect_command(&mut read, &mut Vec::new(), "SELECT \"INBOX\"").await;
        write
            .write_all(
                format!("* OK [UIDVALIDITY 77] selected\r\n{tag} OK selected\r\n").as_bytes(),
            )
            .await
            .unwrap();
        let (tag, command) = scripted_command(&mut read).await;
        assert!(
            command.contains("BODYSTRUCTURE"),
            "unexpected metadata command: {command}"
        );
        write.write_all(format!(
            "* 42 FETCH (UID 42 BODYSTRUCTURE (\"APPLICATION\" \"OCTET-STREAM\" (\"NAME\" \"invoice.command\") NIL NIL \"BASE64\" {} NIL (\"ATTACHMENT\" (\"FILENAME\" \"invoice.command\")) NIL NIL) BODY[HEADER] {{{}}}\r\n{}\r\n)\r\n{tag} OK metadata\r\n",
            encoded_len, headers.len(), headers
        ).as_bytes()).await.unwrap();
        let (tag, command) = scripted_command(&mut read).await;
        let section = command
            .split_once("BODY.PEEK[")
            .and_then(|(_, value)| value.split_once(']').map(|(section, _)| section))
            .unwrap_or("TEXT");
        if section.ends_with(".MIME") {
            write.write_all(format!("* 42 FETCH (UID 42 BODY[{section}] {{{}}}\r\n{}\r\n)\r\n{tag} OK headers\r\n", headers.len(), headers).as_bytes()).await.unwrap();
            let (next_tag, next_command) = scripted_command(&mut read).await;
            let next_section = next_command
                .split_once("BODY.PEEK[")
                .and_then(|(_, value)| value.split_once(']').map(|(section, _)| section))
                .unwrap_or("TEXT");
            write.write_all(format!("* 42 FETCH (UID 42 BODY[{next_section}] {{{}}}\r\n{}\r\n)\r\n{next_tag} OK bytes\r\n", encoded.len(), encoded).as_bytes()).await.unwrap();
        } else {
            write.write_all(format!("* 42 FETCH (UID 42 BODY[{section}] {{{}}}\r\n{}\r\n)\r\n{tag} OK bytes\r\n", encoded.len(), encoded).as_bytes()).await.unwrap();
        }
        let tag = scripted_expect_command(&mut read, &mut Vec::new(), "LOGOUT").await;
        write
            .write_all(format!("* BYE done\r\n{tag} OK logout\r\n").as_bytes())
            .await
            .unwrap();
    });

    let endpoint = imap_test_endpoint(port, true);
    let fetched = service
        .fetch_attachment_at_epoch_with_endpoint_for_test(
            &account,
            "INBOX",
            42,
            &format!(
                "{}:mime-v1:root",
                stable_message_id(account.id, "INBOX", 42)
            ),
            77,
            endpoint,
        )
        .await
        .unwrap();
    assert_eq!(fetched.bytes, raw);
    assert_eq!(fetched.attachment.filename, "invoice.command");
    assert!(
        fetched.attachment.is_potentially_unsafe,
        "classification must survive the selective IMAP fetch"
    );
    assert_eq!(
        fetched.attachment.presentation,
        AttachmentPresentation::Downloadable
    );

    // Dakia intentionally keeps attachment bytes out of SQLite. Persist the
    // display metadata that crosses the core-to-IPC boundary, then prove that
    // the warning remains available after that boundary without retaining a
    // second copy of the opaque file in the database.
    let message_id = stable_message_id(account.id, "INBOX", 42);
    let message = MailSummary {
        id: message_id.clone(),
        account_id: account.id.to_string(),
        mailbox: "INBOX".into(),
        uid: 42,
        mailbox_uid_validity: Some(77),
        message_id: Some("<fictional-attachment@example.test>".into()),
        gmail_message_id: None,
        in_reply_to: None,
        reference_ids: None,
        thread_id: message_id.clone(),
        subject: "Fictional unsafe attachment".into(),
        from_name: Some("Fictional sender".into()),
        from_address: "sender@example.test".into(),
        to_addresses: "reader@example.test".into(),
        cc_addresses: String::new(),
        bcc_addresses: String::new(),
        reply_to_addresses: String::new(),
        received_at: chrono::Utc::now(),
        snippet: "Fictional bytes are available on demand.".into(),
        body_text: "Fictional bytes are available on demand.".into(),
        body_html: None,
        content_state: "complete".into(),
        unsubscribe_kind: None,
        unsubscribe_url: None,
        is_read: true,
        is_flagged: false,
        has_attachments: true,
        category: None,
        classification_confidence: None,
        classification_source: None,
        classification_signals: String::new(),
        attachments: vec![fetched.clone()],
    };
    store.upsert_messages(&[message]).await.unwrap();
    store
        .cache_message_content(
            &message_id,
            false,
            CachedMessageContent {
                body_text: "Fictional bytes are available on demand.".into(),
                body_html: None,
                unsubscribe_kind: None,
                attachments: vec![fetched.attachment.clone()],
            },
        )
        .await
        .unwrap();
    let cached = store
        .cached_message_content(&message_id)
        .await
        .unwrap()
        .expect("attachment metadata survives the cache boundary");
    assert_eq!(cached.attachments.len(), 1);
    assert_eq!(cached.attachments[0].id, fetched.attachment.id);
    assert_eq!(cached.attachments[0].filename, "invoice.command");
    assert!(cached.attachments[0].is_potentially_unsafe);
    assert_eq!(
        serde_json::to_value(&cached.attachments).unwrap()[0]["is_potentially_unsafe"],
        true,
        "the Tauri IPC payload must preserve the visible warning"
    );
    assert!(
        store
            .attachment_data(&message_id, &fetched.attachment.id)
            .await
            .is_err(),
        "opaque attachment bytes must not be copied into SQLite"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn interrupted_attachment_fetch_returns_an_error_before_any_file_can_be_saved() {
    let store = Store::in_memory().await.unwrap();
    let service = MailService::new(store.clone());
    let account = test_account();
    store.save_account(&account).await.unwrap();
    service
        .credentials()
        .set_password(&account, "sync secret")
        .await
        .unwrap();
    store
        .save_mailbox_catalog_state(account.id, "INBOX", "INBOX", 77, 1, true)
        .await
        .unwrap();

    let raw = b"fictional attachment bytes";
    let encoded = STANDARD.encode(raw);
    let headers = concat!(
        "Content-Type: application/octet-stream; name=\"interrupted.command\"\r\n",
        "Content-Disposition: attachment; filename=\"interrupted.command\"\r\n",
        "Content-Transfer-Encoding: base64\r\n\r\n"
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let acceptor = smtp_test_acceptor();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = acceptor.accept(stream).await.unwrap();
        let (read, mut write) = split(stream);
        let mut read = BufReader::new(read);
        write
            .write_all(b"* OK loopback IMAP ready\r\n")
            .await
            .unwrap();
        let tag = scripted_expect_command(&mut read, &mut Vec::new(), "CAPABILITY").await;
        write
            .write_all(format!("* CAPABILITY IMAP4rev1\r\n{tag} OK capability\r\n").as_bytes())
            .await
            .unwrap();
        let tag = scripted_expect_command(
            &mut read,
            &mut Vec::new(),
            "LOGIN \"reader@example.test\" \"sync secret\"",
        )
        .await;
        write
            .write_all(format!("{tag} OK authenticated\r\n").as_bytes())
            .await
            .unwrap();
        let tag = scripted_expect_command(&mut read, &mut Vec::new(), "SELECT \"INBOX\"").await;
        write
            .write_all(
                format!("* OK [UIDVALIDITY 77] selected\r\n{tag} OK selected\r\n").as_bytes(),
            )
            .await
            .unwrap();
        let (tag, command) = scripted_command(&mut read).await;
        assert!(command.contains("BODYSTRUCTURE"));
        write
            .write_all(
                format!(
                    "* 42 FETCH (UID 42 BODYSTRUCTURE (\"APPLICATION\" \"OCTET-STREAM\" (\"NAME\" \"interrupted.command\") NIL NIL \"BASE64\" {} NIL (\"ATTACHMENT\" (\"FILENAME\" \"interrupted.command\")) NIL NIL) BODY[HEADER] {{{}}}\r\n{}\r\n)\r\n{tag} OK metadata\r\n",
                    encoded.len(),
                    headers.len(),
                    headers
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let (tag, command) = scripted_command(&mut read).await;
        let section = command
            .split_once("BODY.PEEK[")
            .and_then(|(_, value)| value.split_once(']').map(|(section, _)| section))
            .unwrap_or("TEXT");
        if section.ends_with(".MIME") {
            write
                .write_all(
                    format!(
                        "* 42 FETCH (UID 42 BODY[{section}] {{{}}}\r\n{}\r\n)\r\n{tag} OK headers\r\n",
                        headers.len(),
                        headers
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let (_, next_command) = scripted_command(&mut read).await;
            let next_section = next_command
                .split_once("BODY.PEEK[")
                .and_then(|(_, value)| value.split_once(']').map(|(section, _)| section))
                .unwrap_or("TEXT");
            write
                .write_all(
                    format!(
                        "* 42 FETCH (UID 42 BODY[{next_section}] {{{}}}\r\n{}",
                        encoded.len(),
                        &encoded[..encoded.len() / 2]
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        } else {
            write
                .write_all(
                    format!(
                        "* 42 FETCH (UID 42 BODY[{section}] {{{}}}\r\n{}",
                        encoded.len(),
                        &encoded[..encoded.len() / 2]
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
        // Drop the TLS stream without completing the announced literal.
    });

    let result = service
        .fetch_attachment_at_epoch_with_endpoint_for_test(
            &account,
            "INBOX",
            42,
            &format!(
                "{}:mime-v1:root",
                stable_message_id(account.id, "INBOX", 42)
            ),
            77,
            imap_test_endpoint(port, true),
        )
        .await;
    assert!(
        result.is_err(),
        "a truncated attachment literal must not return bytes"
    );
    assert!(
        store
            .cached_message_content(&stable_message_id(account.id, "INBOX", 42))
            .await
            .unwrap()
            .is_none(),
        "a failed fetch must not publish attachment content for a later save"
    );
    server.await.unwrap();
}
